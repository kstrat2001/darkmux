//! (#2344) Dispatch-liveness conformance — the enforcement contract #2
//! never had.
//!
//! CLAUDE.md's cross-system contract #2 ("dispatch liveness") binds EVERY
//! production code path that performs model work: it emits the
//! `dispatch.start`/`dispatch.complete` bookends AND, so the live fleet
//! view can say "running now" rather than only "started and never
//! finished", refreshes a `darkmux:session-presence:<sid>` beat for as long
//! as the work is in flight.
//!
//! Until this module the contract was enforced by whoever remembered to
//! grep, and the gap MOVED TWICE while nothing went red: #2344 was filed
//! against six `review.*` kinds, which were then deleted (#2310); the fix
//! that followed covered `dispatch.map`, which no shipped mission config
//! even uses; and three live paths — the radio's local single-shot, the
//! hosted arm of every `darkmux dispatch`, and the `dispatch.single_shot`
//! step kind — still had no beat, one of them with no bookends either.
//! Every one of those states had a green suite.
//!
//! **What this proves, stated honestly.** These tests read darkmux's own
//! source and assert that each model-bearing dispatch site CONTAINS the
//! emitter spawn. That is a structural check: it proves the call is
//! written, not that it fires. Proving it FIRES is the job of the
//! behavioral tests that drive real code against a fake Redis peer
//! (`step_kinds::builtins`'s `dispatch_map_writes_and_releases_a_session_
//! presence_beat` and `dispatch_single_shot_writes_a_session_presence_beat`,
//! and `tests/mock_single_shot_proof.rs` for the entry points). The two
//! halves are complementary: behavioral tests prove the paths they can
//! DRIVE, and this one covers every path INCLUDING the container dispatch
//! that no unit test can drive without Docker.
//!
//! **Coverage, and its edge.** The registry walk sees
//! `StepKindRegistry::with_builtins()`'s five Tier 1 kinds; the roster below
//! adds the two free-function entry points a registry walk structurally
//! cannot see. Two DIFFERENT kinds of gap are NOT enumerated here, and
//! they are not the same gap (corrected #2344 review, CONSIDER 6 — an
//! earlier version of this paragraph conflated them):
//!
//!   1. Genuine Tier 3 kinds in OTHER crates (`darkmux-lab`'s `crawl.unit`,
//!      the binary's `mission.coder`) — this crate structurally cannot see
//!      them at all. Every one of them performs its model work by calling
//!      `darkmux_crew::dispatch::dispatch` (see `crawl::unit_step::
//!      UnitDispatchFn`, whose production value is that function), which
//!      is `dispatch_internal::dispatch`: the `dispatch.internal` row
//!      below covers the call they all route through.
//!   2. `mods.gate`, `records.gather`, and `deliver.github_review` — Tier
//!      1 by classification, physically IN this crate
//!      (`step_kinds/mods_gate.rs`, `records_gather.rs`,
//!      `deliver_github_review.rs`) — just not registered via
//!      `with_builtins()`, so this walk never reaches them. Not a
//!      visibility gap; a registration-path gap. All three currently
//!      declare `SeatClaim::NoModel` (`types.rs`'s own doc names them
//!      there), so nothing is silently uncovered TODAY — but that is this
//!      module reasoning about their `SeatClaim`, not this module
//!      enumerating them. A future change to any of their `seat_claim()`
//!      would not be caught by this sweep. Keying the sweep off
//!      `StepKind::seat_claim()` directly, rather than this hand-
//!      maintained `duty_for_kind` table, would close this end-to-end;
//!      tracked as a follow-up, not done here.
//!
//! The remaining model call in this crate, `probe_remote_endpoint`, is
//! deliberately absent: `doctor --probe` is a 64-token connectivity check
//! with no session id and no bookends, so there is no session for a beat
//! to be the liveness of.
//!
//! Follows #1511/#1979's registry-conformance shape: the table is keyed off
//! `StepKindRegistry::ids()`, and a kind with no row PANICS WITH
//! INSTRUCTIONS. Kind number six has to answer this question before it can
//! ship.

use super::registry::StepKindRegistry;

/// The call every model-bearing path owes.
const PRESENCE_CALL: &str = "session_presence::spawn_session_emitter(";

/// Where a dispatch site's model work actually lives.
struct Site {
    /// Path relative to this crate's manifest dir.
    file: &'static str,
    /// The function whose body must contain [`PRESENCE_CALL`].
    func: &'static str,
    /// A string that MUST appear in the extracted body. Extraction is
    /// brace-matching over real source; this anchor is what turns a drifted
    /// extraction into a loud failure instead of a silent pass against the
    /// wrong text.
    anchor: &'static str,
}

/// What a registered step kind owes contract #2.
enum Duty {
    /// Declares no model work — there is nothing to be live about.
    NoModelWork,
    /// Performs model work, in the named function.
    ModelWork(Site),
}

/// The row every registered kind must have. `None` ⇒ no row ⇒ the test
/// panics naming the kind, which is the point.
fn duty_for_kind(kind_id: &str) -> Option<Duty> {
    Some(match kind_id {
        // Delegates the whole dispatch to `dispatch_internal::dispatch` —
        // the container path (and, for an endpoint-bearing profile, the
        // hosted one below). Not unit-drivable: it spawns a real Docker
        // container, which is why the structural half of this module
        // exists at all.
        "dispatch.internal" => Duty::ModelWork(Site {
            file: "src/dispatch_internal.rs",
            func: "dispatch",
            anchor: "darkmux-dispatch-",
        }),
        "dispatch.single_shot" => Duty::ModelWork(Site {
            file: "src/step_kinds/builtins.rs",
            func: "run_single_shot",
            anchor: "dispatch.single_shot (local)",
        }),
        "dispatch.map" => Duty::ModelWork(Site {
            file: "src/step_kinds/builtins.rs",
            func: "run_map",
            anchor: "serializing dispatch.map results",
        }),
        // Runs a shell command / nothing at all. No model, no session.
        "procedural.shell" | "procedural.noop" => Duty::NoModelWork,
        _ => return None,
    })
}

/// The model-bearing dispatch functions reachable WITHOUT a step kind —
/// the CLI verb, the fleet queue, and `darkmux acp`'s radio seats all enter
/// here. A registry walk cannot see these, and that is exactly how
/// `dispatch_local_single_shot` and `dispatch_remote` stayed uncovered
/// through two passes at #2344.
const ENTRY_POINTS: &[Site] = &[
    // The radio router + answering seats (`src/radio.rs` via
    // `darkmux_fleet::routing::dispatch_routed_via`).
    Site {
        file: "src/dispatch_internal.rs",
        func: "dispatch_local_single_shot",
        anchor: "single-shot dispatch requires one",
    },
    // The hosted arm of every `darkmux dispatch` and fleet-queue job.
    Site {
        file: "src/dispatch_internal.rs",
        func: "dispatch_remote",
        anchor: "dispatch_remote requires a remote endpoint",
    },
];

fn read_src(file: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(file);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {} for liveness conformance: {e}", path.display()))
}

/// Extract `func`'s body from `src` by matching braces from its signature's
/// opening `{`, skipping over comments, string literals (raw ones included)
/// and char literals so a `'{'` or a `"}"` inside the body cannot end the
/// scan early.
///
/// Deliberately a small lexer rather than "scan to the next line that is
/// exactly `}`": the two shapes this module covers sit at different
/// indentation levels (free functions at column 0, step-kind methods inside
/// an `impl`), and a line-based rule that handles both is a rule that
/// handles neither reliably.
fn fn_body(src: &str, func: &str) -> String {
    let decl = format!("fn {func}(");
    let start = src
        .find(&decl)
        .unwrap_or_else(|| panic!("no `{decl}` in source — did the function get renamed?"));
    let cs: Vec<char> = src[start..].chars().collect();
    let mut i = 0usize;
    let mut depth = 0i32;
    let mut body_start: Option<usize> = None;
    while i < cs.len() {
        let c = cs[i];
        let next = cs.get(i + 1).copied();
        match c {
            '/' if next == Some('/') => {
                while i < cs.len() && cs[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            '/' if next == Some('*') => {
                i += 2;
                while i + 1 < cs.len() && !(cs[i] == '*' && cs[i + 1] == '/') {
                    i += 1;
                }
                i += 2;
                continue;
            }
            // Raw string: r"..." / r#"..."# / r##"..."##. Its terminator is
            // `"` + the same number of `#`s, and nothing inside it escapes —
            // so it has to be skipped whole or a `{` in a JSON fixture ends
            // the scan in the wrong place.
            'r' if next == Some('"') || next == Some('#') => {
                let mut hashes = 0usize;
                let mut j = i + 1;
                while cs.get(j) == Some(&'#') {
                    hashes += 1;
                    j += 1;
                }
                if cs.get(j) != Some(&'"') {
                    i += 1; // an identifier that merely starts with `r`
                    continue;
                }
                j += 1;
                loop {
                    if j >= cs.len() {
                        break;
                    }
                    if cs[j] == '"' && (1..=hashes).all(|k| cs.get(j + k) == Some(&'#')) {
                        j += 1 + hashes;
                        break;
                    }
                    j += 1;
                }
                i = j;
                continue;
            }
            '"' => {
                i += 1;
                while i < cs.len() && cs[i] != '"' {
                    if cs[i] == '\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i += 1;
                continue;
            }
            '\'' => {
                // A char literal is `'x'` or `'\x'`; anything else starting
                // with `'` is a lifetime, which carries no braces.
                let is_char_lit = match next {
                    Some('\\') => true,
                    Some(_) => cs.get(i + 2) == Some(&'\''),
                    None => false,
                };
                if is_char_lit {
                    i += 1;
                    while i < cs.len() && cs[i] != '\'' {
                        if cs[i] == '\\' {
                            i += 1;
                        }
                        i += 1;
                    }
                }
                i += 1;
                continue;
            }
            '{' => {
                depth += 1;
                if body_start.is_none() {
                    body_start = Some(i);
                }
            }
            '}' => {
                depth -= 1;
                if let (0, Some(b)) = (depth, body_start) {
                    return cs[b..=i].iter().collect();
                }
            }
            _ => {}
        }
        i += 1;
    }
    panic!("unbalanced braces scanning `{func}` — the extractor needs fixing, not the assertion");
}

/// Assert one site spawns a beat, with the anchor guarding the extraction.
fn assert_site_spawns_a_beat(site: &Site, what: &str) {
    let src = read_src(site.file);
    let body = fn_body(&src, site.func);
    assert!(
        body.contains(site.anchor),
        "extraction drift: `{}` in {} does not contain its anchor {:?}. Fix the anchor (or \
         the extractor) before trusting anything else this test says.",
        site.func,
        site.file,
        site.anchor,
    );
    assert!(
        body.contains(PRESENCE_CALL),
        "{what} performs model work but `{}` in {} never calls `{PRESENCE_CALL}`.\n\n\
         Contract #2 (dispatch liveness, CLAUDE.md) binds every production path that \
         performs model work: emit the dispatch.start/complete bookends AND refresh a \
         session-presence beat while the work is in flight, keyed on the SAME session_id \
         the path's own records use. Without the beat the live fleet view cannot show this \
         work as running — it sees a start, then nothing, for however long the model takes.\n\n\
         Spawn `darkmux_flow::session_presence::spawn_session_emitter(...)` before the first \
         model call and `stop()` it before the terminal record; `SessionEmitter::drop` is \
         the backstop for a `?`/panic in between.",
        site.func,
        site.file,
    );
}

#[test]
fn every_registered_step_kind_that_dispatches_spawns_a_session_presence_beat() {
    let registry = StepKindRegistry::with_builtins();
    for id in registry.ids() {
        let duty = duty_for_kind(&id).unwrap_or_else(|| {
            panic!(
                "step kind `{id}` is registered but has no row in `duty_for_kind`. Add one: \
                 `Duty::NoModelWork` if it never calls a model, or `Duty::ModelWork(Site {{ \
                 .. }})` naming the function that performs the model work — which must spawn \
                 a session-presence beat (contract #2, dispatch liveness). Do not add the row \
                 without reading the kind's `run`/`run_streaming`: the whole reason this test \
                 exists is that the gap moved twice while every suite stayed green.",
            )
        });
        match duty {
            Duty::NoModelWork => {}
            Duty::ModelWork(site) => {
                assert_site_spawns_a_beat(&site, &format!("step kind `{id}`"))
            }
        }
    }
}

#[test]
fn every_dispatch_entry_point_spawns_a_session_presence_beat() {
    for site in ENTRY_POINTS {
        assert_site_spawns_a_beat(site, &format!("dispatch entry point `{}`", site.func));
    }
}

#[test]
fn the_extractor_discriminates_between_neighboring_functions() {
    // The load-bearing negative control. Without it, an extractor that
    // silently ran off the end of a function and swallowed the whole file
    // would report every site as conformant — a probe that passes without
    // actually testing anything, which is worse than no probe.
    let src = read_src("src/dispatch_internal.rs");
    let remote = fn_body(&src, "dispatch_remote");
    assert!(
        remote.contains("dispatch_remote requires a remote endpoint"),
        "sanity: the extracted body is `dispatch_remote`'s own"
    );
    assert!(
        !remote.contains("single-shot dispatch requires one"),
        "`dispatch_remote`'s body ran on into its neighbor `dispatch_local_single_shot` — \
         the extractor is over-reaching, so every other assertion here is unearned"
    );

    // And the other direction: a genuinely model-free function must NOT
    // report a beat, or a `true` here means nothing.
    let pure = fn_body(&read_src("src/step_kinds/builtins.rs"), "conservative_hosted_spend");
    assert!(
        !pure.contains(PRESENCE_CALL),
        "a pure helper reported a presence beat — the extractor is over-reaching"
    );
}
