//! (#2534) Conformance: every `lms` spawn in the workspace pins its cwd.
//!
//! #1863 pinned every `lms`-spawning `Command`'s working directory to `/`
//! so a daemon or dispatch started from a git worktree that later gets
//! removed doesn't crash outright. The chokepoint (`run_bounded`, in
//! `crate::gestalt_host::lms_host`) got a behavioral test
//! (`run_bounded_survives_a_deleted_cwd`) that actually deletes a cwd and
//! spawns through it. Three spawns bypass that chokepoint by construction —
//! `darkmux-profiles::lms::load_with_identifier` (bespoke stdio-inheriting
//! load loop), `darkmux-crew::dispatch_internal::probe_loaded_model_list`
//! (a direct `lms ps --json` shell-out on the dispatch path), and
//! `darkmux-lab::lab::scores::MachineFingerprint::detect` (a lab-run
//! hardware fingerprint) — and each carried its OWN copy of
//! `cmd.current_dir("/")` with nothing protecting it. Proven: DELETING
//! `load_with_identifier`'s pin line and running `cargo test -p
//! darkmux-profiles` left all 213 tests green.
//!
//! #2534's fix names the pin — `darkmux_profiles::lms::pin_cwd` — and calls
//! it from all four sites (the three bypasses plus `run_bounded` itself, so
//! there is exactly ONE place the rule is written). This module is the test
//! that keeps a FIFTH spawn from joining unnoticed.
//!
//! **What this is: a lint, not an enumeration.** Rather than a
//! hand-maintained roster of today's known sites (which a fifth spawn
//! simply wouldn't be added to), this walks every `.rs` file under the
//! three crates' `src/` trees and finds every line matching one of the
//! literal shapes this codebase actually spawns `lms` with —
//! `Command::new("lms")`, `Command::new(lms_bin())`, and
//! `Command::new(darkmux_types::config_access::lms_bin())` (the last added
//! by #1939, which resolved `dispatch_internal.rs` and `scores.rs` off the
//! fully-qualified accessor call rather than the crate-local `lms_bin()`
//! wrapper the other two use) — the shapes the greps behind #2534 and #1939
//! found. For each match, it asserts the ENCLOSING function calls
//! `pin_cwd(` (directly) or `run_bounded(` (transitively — `run_bounded`'s
//! own pin is checked separately, below). A new `lms` spawn anywhere in
//! these three crates, written the way every `lms` spawn in this codebase
//! already is, is caught by this scan without anyone adding a row for it.
//!
//! That is the whole of the claim, and it is deliberately modest: this is
//! **best-effort**. It catches the shapes this codebase currently writes,
//! and it cannot catch a spawn built any other way. A source scan reads
//! text; it does not know types, or control flow, or whether the `Command`
//! it found is the one that eventually runs.
//!
//! **Scope, stated honestly.** FOUR live `lms`-spawning sites are outside
//! what this text scan can see:
//!
//!   - `LmsHost::ps_bounded` / `list_catalog` / `load` / `unload` — three
//!     `Command::new(&self.bin)` sites in `crate::gestalt_host::lms_host`.
//!     `self.bin` is a struct field, which a text scan can't tell from an
//!     unrelated `Command::new(&self.something)` elsewhere without
//!     hardcoding `LmsHost`'s type.
//!   - `crate::model_ledger::bounded_stdout` (`Command::new(bin)`, in this
//!     crate's `model_ledger.rs`), reached live from `gather()` →
//!     `gather_with_bin` for the `lms` probe. The binary is bound to a
//!     local BEFORE the spawn, so neither literal shape ever appears on the
//!     spawn line.
//!
//! All four are pinned TRANSITIVELY, verified by reading each: every one
//! hands its `cmd` straight to `run_bounded(cmd, …)`, whose own pin is
//! checked directly by `run_bounded_itself_pins_cwd_via_the_named_helper`
//! below. So there is no live bug here. But `bounded_stdout` is also a
//! working TEMPLATE for evading this scan, sitting in the very crate that
//! owns the guard: bind the binary to a local first and the pattern match
//! never fires. If a future site spawns that way WITHOUT reaching
//! `run_bounded`, this scan will not catch it — which is why
//! `run_bounded_survives_a_deleted_cwd` (behavioral, in `lms_host.rs`
//! itself) stays in place rather than being retired in favor of this
//! structural one.
//!
//! **The durable fix is not a better scan.** It is to make an unpinned
//! spawn UNREPRESENTABLE rather than detectable — a newtype whose only
//! constructor pins the working directory, so the raw form this module
//! greps for never appears in the tree to be scanned for. Filed as #2572;
//! deliberately out of scope here.
//!
//! Also out of scope, per the issue: #2532's `procedural.shell` spawn
//! (a different, non-`lms` step kind — this scan's patterns don't match it
//! and it needs a different fix) and #2533 (`lms_bin` can resolve to a
//! relative path, which `pin_cwd`'s `/` doesn't address).

use std::path::{Path, PathBuf};

/// The literal shapes this codebase spawns `lms` with (verified by grep
/// against #2534's and #1939's filed sites — see the module doc).
const LMS_SPAWN_PATTERNS: &[&str] = &[
    "Command::new(\"lms\")",
    "Command::new(lms_bin())",
    "Command::new(darkmux_types::config_access::lms_bin())",
];

/// The call every `lms` spawn owes, one way or another.
const PIN_CALL: &str = "pin_cwd(";
/// Flowing into this chokepoint also satisfies the pin — its OWN body is
/// checked directly by `run_bounded_itself_pins_cwd_via_the_named_helper`.
const CHOKEPOINT_CALL: &str = "run_bounded(";

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The CODE half of a line — everything before its first `//`.
///
/// Every check in this module runs on this, never on raw source. Without
/// it the guard is defeated by a comment: an adversarial review of #2534
/// wrote an unpinned spawn whose enclosing function merely SAID
/// `// TODO: we should call pin_cwd(&mut cmd) here once #9999 lands.`,
/// and the whole module went green. Same reason `///` doc comments (which
/// in these files routinely name `run_bounded(`) must not count as a call.
///
/// Known limit, in keeping with this module being a lint: a `//` inside a
/// string literal earlier on the same line blinds the rest of that line.
/// What that costs is a real call being MISSED — a loud assertion failure,
/// not a silent pass — except in the contrived case where the blinded
/// remainder is itself the spawn.
fn code_only(line: &str) -> &str {
    match line.find("//") {
        Some(i) => &line[..i],
        None => line,
    }
}

/// Every `.rs` file under `root`, recursively. Panics (rather than
/// swallowing the error) if `root` itself doesn't exist — a typo'd sibling
/// path must not silently sweep zero files and pass vacuously.
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

/// True when this line DECLARES a function, at any visibility or qualifier
/// combination Rust allows: `fn`, `pub`, `pub(crate)`, `pub(super)`,
/// `pub(in path::to)`, `const`, `unsafe`, `async`, `extern "C"`, in any
/// legal order.
///
/// The first version of this recognized five hardcoded prefixes and missed
/// `pub(super) fn`, `unsafe fn`, `const fn` and friends — all present in
/// the swept crates. Only `find_fn_decl` uses it now (`same_function_window`
/// bounds on braces instead, so no declaration form can widen a window),
/// but a missed form there would still silently stop checking a renamed
/// anchor.
fn is_fn_decl_line(line: &str) -> bool {
    let mut t = code_only(line).trim_start();
    loop {
        if let Some(rest) = t.strip_prefix("pub") {
            let rest = rest.trim_start();
            // `pub(crate)` / `pub(super)` / `pub(in crate::x)`
            let rest = if rest.starts_with('(') {
                match rest.find(')') {
                    Some(i) => &rest[i + 1..],
                    None => return false,
                }
            } else {
                rest
            };
            t = rest.trim_start();
            continue;
        }
        // `extern "C"` — the ABI string, if one follows.
        if let Some(rest) = t.strip_prefix('"') {
            match rest.find('"') {
                Some(i) => {
                    t = rest[i + 1..].trim_start();
                    continue;
                }
                None => return false,
            }
        }
        let mut advanced = false;
        for kw in ["const ", "unsafe ", "async ", "extern ", "default "] {
            if let Some(rest) = t.strip_prefix(kw) {
                t = rest.trim_start();
                advanced = true;
                break;
            }
        }
        if !advanced {
            break;
        }
    }
    t.starts_with("fn ")
}

/// From `start_idx` (0-based, the line an `lms` spawn was matched on)
/// forward to the end of the enclosing block — the same-function text a
/// real cwd pin has to appear inside. Returns COMMENT-STRIPPED text; see
/// `code_only`.
///
/// Bounded on BRACE DEPTH, not on "the next line that looks like a `fn`
/// declaration". The first version used the latter and an adversarial
/// review of #2534 walked straight through it: its five hardcoded
/// declaration prefixes missed `pub(super) fn`, so an unpinned spawn whose
/// neighbor happened to be declared `pub(super) fn` had that neighbor's
/// `pin_cwd` call read as its own. Depth needs no roster of declaration
/// forms — and it ends the window at the closing brace, so a following
/// function's `///` doc comment naming `run_bounded(` falls outside it too.
///
/// Narrowing this accepts, deliberately: a spawn nested inside an inner
/// block ends its window at THAT block's close, so a pin written after the
/// block would be missed. That direction is safe — it fails loud rather
/// than passing quietly — and no current site is written that way.
///
/// Works from either end of a function: started ON a declaration line the
/// window closes when depth returns to 0 (the body's own closing brace);
/// started INSIDE a body — where the enclosing brace opened before
/// `start_idx` and is never counted — it closes when depth first goes
/// negative.
fn same_function_window(lines: &[&str], start_idx: usize) -> String {
    let mut depth: i32 = 0;
    let mut entered = false;
    let mut end = lines.len();
    for (offset, line) in lines[start_idx..].iter().enumerate() {
        for ch in code_only(line).chars() {
            match ch {
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
            if depth > 0 {
                entered = true;
            }
        }
        if (entered && depth <= 0) || depth < 0 {
            end = start_idx + offset + 1;
            break;
        }
    }
    lines[start_idx..end]
        .iter()
        .map(|l| code_only(l))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The 0-based line declaring `fn <name>(` (any visibility/qualifier), or
/// panic — a renamed function must fail loud, not silently stop being
/// checked.
fn find_fn_decl(lines: &[&str], name: &str) -> usize {
    let needle = format!("fn {name}(");
    lines
        .iter()
        .position(|l| is_fn_decl_line(l) && code_only(l).contains(&needle))
        .unwrap_or_else(|| panic!("no `fn {name}(` found — did it get renamed?"))
}

struct Finding {
    file: PathBuf,
    line: usize, // 1-based
    text: String,
}

/// Every `lms`-spawn match (regardless of whether it's pinned), across all
/// `.rs` files under `root`. Used both by the main assertion (filtered to
/// unpinned ones) and by the sanity check that the scan actually found
/// something.
fn all_lms_spawn_matches(root: &Path) -> Vec<(PathBuf, usize, bool)> {
    let mut matches = Vec::new();
    for file in rust_files(root) {
        let src = std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("reading {}: {e}", file.display()));
        let lines: Vec<&str> = src.lines().collect();
        for (idx, line) in lines.iter().enumerate() {
            if LMS_SPAWN_PATTERNS.iter().any(|p| code_only(line).contains(p)) {
                let window = same_function_window(&lines, idx);
                let pinned = window.contains(PIN_CALL) || window.contains(CHOKEPOINT_CALL);
                matches.push((file.clone(), idx + 1, pinned));
            }
        }
    }
    matches
}

fn sweep_roots() -> Vec<PathBuf> {
    vec![
        manifest_dir().join("src"),
        manifest_dir().join("../darkmux-crew/src"),
        manifest_dir().join("../darkmux-lab/src"),
    ]
}

#[test]
fn every_lms_spawn_in_the_swept_crates_pins_its_cwd() {
    let mut findings: Vec<Finding> = Vec::new();
    for root in sweep_roots() {
        for (file, line, pinned) in all_lms_spawn_matches(&root) {
            if !pinned {
                let src = std::fs::read_to_string(&file).unwrap();
                let text = src.lines().nth(line - 1).unwrap_or("").trim().to_string();
                findings.push(Finding { file, line, text });
            }
        }
    }

    assert!(
        findings.is_empty(),
        "found {} `lms` spawn(s) with no cwd pin in the same function (#2534 — #1863 pinned \
         every spawn's cwd but only the `run_bounded` chokepoint had a test; these bypass it):\n{}\n\n\
         Fix: call `darkmux_profiles::lms::pin_cwd(&mut cmd)` right after constructing the \
         `Command`, or route the call through `run_bounded` if it's a darkmux-profiles-internal \
         spawn that can accept that runner's stdio/deadline shape.",
        findings.len(),
        findings.iter().map(|f| format!("  {}:{}: {}", f.file.display(), f.line, f.text)).collect::<Vec<_>>().join("\n"),
    );
}

/// Negative control for the scan itself: a scanner whose patterns typo'd,
/// or whose sweep roots resolved to an empty/wrong directory, would report
/// the assertion above as vacuously green — a probe that passes without
/// testing anything, worse than no probe. Assert the scan actually SEES
/// the sites #2534 was filed against (at least 7 matches: 5 in `lms.rs`,
/// 1 in `dispatch_internal.rs`, 1 in `scores.rs`, per the grep behind this
/// fix). #1939 later rewrote the `dispatch_internal.rs` and `scores.rs`
/// sites to resolve through the fully-qualified
/// `darkmux_types::config_access::lms_bin()` rather than the crate-local
/// `lms_bin()` wrapper — a third literal shape was added to
/// `LMS_SPAWN_PATTERNS` above to keep seeing them; the expected total stays
/// 7, only the matched literal text for two of the sites changed.
#[test]
fn the_scan_actually_finds_the_known_sites() {
    let total: usize = sweep_roots().iter().map(|r| all_lms_spawn_matches(r).len()).sum();
    assert!(
        total >= 7,
        "the conformance scan found only {total} `lms` spawn(s) across the swept crates — \
         expected at least 7 (the known #2534 sites). Either the sweep roots are wrong, the \
         `LMS_SPAWN_PATTERNS` no longer match this codebase's spawn style, or a site was \
         deleted — in every case, `every_lms_spawn_in_the_swept_crates_pins_its_cwd` above is \
         not actually testing what it claims to.",
    );
}

/// Second negative control: prove the window extractor doesn't run off the
/// end of the function it's meant to be reading and swallow a NEIGHBOR's
/// pin call, which would make every assertion above pass regardless of
/// whether the matched site itself is protected.
#[test]
fn the_window_extractor_does_not_leak_into_a_neighboring_function() {
    let src = std::fs::read_to_string(manifest_dir().join("src/lms.rs")).unwrap();
    let lines: Vec<&str> = src.lines().collect();

    // `unload` (no `lms`-spawn pattern issue here, but it precedes
    // `load_with_identifier` and must not see INTO it) is a real anchor:
    // its body must not contain `load_with_identifier`'s signature.
    let unload_idx = find_fn_decl(&lines, "unload");
    let unload_window = same_function_window(&lines, unload_idx);
    assert!(
        !unload_window.contains("fn load_with_identifier"),
        "the window extractor read past `unload`'s own body into its neighbor — \
         `same_function_window` is over-reaching, so every other assertion in this \
         module is unearned"
    );
    assert!(unload_window.contains("run_bounded("), "sanity: `unload`'s own body is what got extracted");
}

/// Third negative control, and the one the first version was missing: the
/// leak test above only covers TOTAL destruction of the window (running
/// past a neighbor declared `pub fn`). It said nothing about the window
/// being widened by a declaration form the extractor didn't RECOGNIZE —
/// which is exactly how an adversarial review of #2534 got an unpinned
/// spawn past the whole module, by giving it a `pub(super) fn` neighbor.
///
/// Bounding on brace depth is what fixed that; this is the assertion that
/// keeps it fixed, across every declaration form the swept crates write.
#[test]
fn the_window_extractor_stops_before_a_neighbor_in_any_declaration_form() {
    let forms = [
        "fn",
        "pub fn",
        "pub(crate) fn",
        "pub(super) fn",
        "pub(in crate::lms) fn",
        "unsafe fn",
        "pub unsafe fn",
        "const fn",
        "pub const fn",
        "async fn",
        "pub async fn",
        "extern \"C\" fn",
    ];
    for form in forms {
        let src = format!(
            "pub fn spawner() -> Result<()> {{\n\
             \x20   let mut cmd = Command::new(\"lms\");\n\
             \x20   cmd.args([\"ps\"]);\n\
             \x20   Ok(())\n\
             }}\n\
             \n\
             /// A doc comment that happens to mention run_bounded( and pin_cwd(.\n\
             {form} unrelated_neighbor(cmd: &mut Command) {{\n\
             \x20   pin_cwd(cmd);\n\
             }}\n"
        );
        let lines: Vec<&str> = src.lines().collect();
        let idx = lines
            .iter()
            .position(|l| l.contains(LMS_SPAWN_PATTERNS[0]))
            .expect("the synthetic fixture must contain the spawn shape being tested");
        let window = same_function_window(&lines, idx);
        assert!(
            !window.contains(PIN_CALL) && !window.contains(CHOKEPOINT_CALL),
            "`same_function_window` ran past its own function into a neighbor declared \
             `{form}` — an unpinned spawn with a neighbor in that form reads as pinned, and \
             `every_lms_spawn_in_the_swept_crates_pins_its_cwd` is unearned.\n\
             window was:\n{window}"
        );
    }
}

/// Fourth negative control: a COMMENT naming the helper must not satisfy
/// the guard. The check ran on raw source lines before, and an adversarial
/// review of #2534 defeated the whole module with
/// `// TODO: we should call pin_cwd(&mut cmd) here once #9999 lands.` — the
/// exact spawn shape the scan matches, entirely unpinned, four tests green.
///
/// This also covers the everyday form of the same bug: `///` doc comments
/// naming `run_bounded(` are common in these files, so a neighbor's
/// documentation could otherwise pin a spawn by accident.
#[test]
fn a_comment_naming_the_pin_does_not_satisfy_the_guard() {
    let cases = [
        "    // TODO: we should call pin_cwd(&mut cmd) here once #9999 lands.",
        "    /// routed through run_bounded( elsewhere",
        "    cmd.args([\"ps\"]); // pin_cwd(&mut cmd) is handled by the caller",
    ];
    for comment in cases {
        let src = format!(
            "pub fn spawner() -> Result<()> {{\n\
             \x20   let mut cmd = Command::new(\"lms\");\n\
             {comment}\n\
             \x20   Ok(())\n\
             }}\n"
        );
        let lines: Vec<&str> = src.lines().collect();
        let idx = lines
            .iter()
            .position(|l| l.contains(LMS_SPAWN_PATTERNS[0]))
            .expect("the synthetic fixture must contain the spawn shape being tested");
        let window = same_function_window(&lines, idx);
        assert!(
            !window.contains(PIN_CALL) && !window.contains(CHOKEPOINT_CALL),
            "a comment satisfied the cwd-pin guard — `code_only` is no longer stripping comment \
             text, so an unpinned spawn passes as long as some line near it MENTIONS the \
             helper.\n  comment: {comment}\n  window was:\n{window}"
        );
    }
}

/// `run_bounded` never matches `LMS_SPAWN_PATTERNS` (it receives an
/// already-constructed `Command` as a parameter, it doesn't build one) — so
/// the scan above cannot verify its pin. Every caller-side assertion in
/// this module treats "flows into `run_bounded`" as sufficient; this is
/// the direct check that trust is earned.
#[test]
fn run_bounded_itself_pins_cwd_via_the_named_helper() {
    let src = std::fs::read_to_string(manifest_dir().join("src/gestalt_host/lms_host.rs")).unwrap();
    let lines: Vec<&str> = src.lines().collect();
    let idx = find_fn_decl(&lines, "run_bounded");
    let window = same_function_window(&lines, idx);
    assert!(
        window.contains(PIN_CALL),
        "`run_bounded` (the #1863 chokepoint) no longer calls `pin_cwd(` — every caller that \
         relies on flowing through it for its cwd pin (this module's main scan treats \
         `run_bounded(` as sufficient) is now unprotected too."
    );
}
