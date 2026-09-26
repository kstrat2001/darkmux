//! (#2902 step 1a) Usage-record conformance: the roster of every host-side
//! model call path, and the sweep that makes a new one visibly missing.
//!
//! The rule: every call darkmux makes to a model endpoint emits EXACTLY ONE
//! `telemetry.tokens` record (`crate::usage`) when its reply returns.
//!
//! Two halves, the same shape as `liveness_conformance`:
//!
//! - **Behavioral.** Each path in [`ROSTER`] names the test that DRIVES it
//!   once against a mock endpoint and asserts exactly one record carrying the
//!   canonical fields (`usage::assert_one_usage_record`, or its inlined twin
//!   in the integration test that cannot reach a `#[cfg(test)]` helper).
//! - **Structural.** [`every_transport_call_site_is_on_the_roster`] walks
//!   EVERY source file of this crate and counts each production call of the
//!   transports (and refuses a `use … as` alias that would hide one). A new call site
//!   changes a count and fails here with instructions, so a path cannot ship
//!   without deciding its usage record. [`every_roster_path_calls_the_writer`]
//!   checks each emitting path's body reaches the one writer.
//!
//! Scope: host-side only. The runtime's own calls (turns and compaction,
//! #2902 step 1b) are rostered in `runtime/src/usage_conformance.rs`; their
//! records are written here by the container tailer (`handle_event`'s
//! `model.completed` and `compaction.call` arms), driven by
//! `usage_conformance_container_turn` and `usage_conformance_compaction_call`. No other workspace crate
//! calls a transport directly: `darkmux-lab` and the binary reach models
//! through `dispatch::dispatch` / `dispatch_local_single_shot` (checked
//! with a grep when this roster was written).

use super::liveness_conformance::{block_from, fn_body, read_src};

/// What a transport call site owes.
enum Duty {
    /// Emits one usage record per call. `writer_in` is the function whose
    /// body calls the writer (the caller of the transport, or the loop that
    /// consumes its result); `test` names the behavioral test that drives it.
    Emits {
        writer_in: (&'static str, &'static str),
        test: &'static str,
    },
    /// A transport itself (it has no session to attribute a record to; its
    /// callers emit).
    Transport,
    /// Deliberately emits none, with the reason.
    Exempt(&'static str),
}

struct CallSite {
    /// Path relative to this crate's manifest dir.
    file: &'static str,
    /// The function containing the call.
    caller: &'static str,
    /// The transport it calls.
    transport: &'static str,
    duty: Duty,
}

const ROSTER: &[CallSite] = &[
    CallSite {
        file: "src/single_shot.rs",
        caller: "single_shot_chat",
        transport: "remote_chat_completion(",
        duty: Duty::Transport,
    },
    CallSite {
        file: "src/single_shot.rs",
        caller: "single_shot_chat_hosted",
        transport: "remote_chat_completion(",
        duty: Duty::Transport,
    },
    CallSite {
        file: "src/dispatch_internal.rs",
        caller: "dispatch_remote",
        transport: "remote_chat_completion(",
        duty: Duty::Emits {
            writer_in: ("src/dispatch_internal.rs", "dispatch_remote"),
            test: "dispatch_internal::tests::usage_conformance_dispatch_remote",
        },
    },
    CallSite {
        file: "src/dispatch_internal.rs",
        caller: "probe_remote_endpoint",
        transport: "remote_chat_completion(",
        duty: Duty::Exempt(
            "`doctor --probe`: a 64-token connectivity check with no session id and no \
             bookends, so a usage record would belong to no run. Open question for #2902.",
        ),
    },
    CallSite {
        file: "src/dispatch_internal.rs",
        caller: "dispatch_local_single_shot",
        transport: "single_shot_chat(",
        duty: Duty::Emits {
            writer_in: ("src/dispatch_internal.rs", "dispatch_local_single_shot"),
            test: "tests/mock_single_shot_proof.rs::container_free_single_shot_dispatch_round_trips_through_a_real_http_mock_server",
        },
    },
    CallSite {
        file: "src/step_kinds/builtins.rs",
        caller: "run_single_shot",
        transport: "single_shot_chat_hosted(",
        duty: Duty::Emits {
            writer_in: ("src/step_kinds/builtins.rs", "run_single_shot"),
            test: "step_kinds::builtins::tests::usage_conformance_single_shot_step_hosted",
        },
    },
    CallSite {
        file: "src/step_kinds/builtins.rs",
        caller: "run_single_shot",
        transport: "single_shot_chat(",
        duty: Duty::Emits {
            writer_in: ("src/step_kinds/builtins.rs", "run_single_shot"),
            test: "step_kinds::builtins::tests::usage_conformance_single_shot_step_local",
        },
    },
    CallSite {
        file: "src/step_kinds/builtins.rs",
        caller: "map_hosted_dispatch",
        transport: "single_shot_chat_hosted(",
        duty: Duty::Emits {
            writer_in: ("src/step_kinds/builtins.rs", "run_map"),
            test: "step_kinds::builtins::tests::usage_conformance_map_item_hosted",
        },
    },
    CallSite {
        file: "src/step_kinds/builtins.rs",
        caller: "map_local_item",
        transport: "single_shot_chat(",
        duty: Duty::Emits {
            writer_in: ("src/step_kinds/builtins.rs", "run_map"),
            test: "step_kinds::builtins::tests::usage_conformance_map_item_local",
        },
    },
];

/// The container path is not a transport call in this crate (the runtime
/// makes the HTTP call inside Docker); its record is written by the host
/// tailer from each `model.completed` event, driven by
/// `dispatch_internal::tests::usage_conformance_container_turn`.
const CONTAINER_TURN: (&str, &str) = ("src/dispatch_internal.rs", "handle_event");

const TRANSPORTS: &[&str] = &[
    "remote_chat_completion(",
    "single_shot_chat(",
    "single_shot_chat_hosted(",
];

/// Test-only source files that name the transports as DATA (this roster, the
/// liveness roster), never as calls.
const TEST_ONLY_FILES: &[&str] = &[
    "src/step_kinds/usage_conformance.rs",
    "src/step_kinds/liveness_conformance.rs",
];

/// The production part of a source file: the file with its unit-test module
/// (`mod tests { … }`, brace-matched, so production code AFTER the module
/// still counts) cut out, and comment lines dropped. A `#[path]`-included
/// test file (`*_tests.rs`) is excluded by [`crate_sources`] instead.
fn production_lines(src: &str) -> Vec<String> {
    let mut prod = src.to_string();
    while let Some(at) = prod.find("\nmod tests {") {
        let block = block_from(&prod[at..], "mod tests {");
        let end = at
            + prod[at..]
                .find(&block)
                .expect("block_from returns a slice of its input")
            + block.len();
        prod.replace_range(at..end, "");
    }
    prod.lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .map(str::to_string)
        .collect()
}

/// Calls of `transport` on `line`: every occurrence except a declaration
/// (`fn <transport>`), so a call sharing a line with some OTHER signature
/// still counts.
fn calls_on_line(line: &str, transport: &str) -> usize {
    line.matches(transport).count() - line.matches(&format!("fn {transport}")).count()
}

/// Every `.rs` file under this crate's `src/`, as a manifest-relative path,
/// minus `*_tests.rs` files and the test-only rosters.
fn crate_sources() -> Vec<String> {
    fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, root, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                if !rel.ends_with("_tests.rs") && !TEST_ONLY_FILES.contains(&rel.as_str()) {
                    out.push(rel);
                }
            }
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut out = Vec::new();
    walk(&root.join("src"), root, &mut out);
    out.sort();
    out
}

/// Every transport call in EVERY source file of this crate is on the roster
/// (a file the roster never names must have none), and no file imports a
/// transport under another name, which would hide its calls from the count.
#[test]
fn every_transport_call_site_is_on_the_roster() {
    let files = crate_sources();
    assert!(
        files.iter().any(|f| f == "src/dispatch.rs"),
        "the walk must see the whole crate: {files:?}"
    );
    for file in &files {
        let src = read_src(file);
        let lines = production_lines(&src);
        for transport in TRANSPORTS {
            let name = transport.trim_end_matches('(');
            for l in &lines {
                assert!(
                    !l.contains(&format!("{name} as ")),
                    "{file}: `{}` imports the transport `{name}` under another name, which hides \
                     its calls from this roster (#2902); call it by its own name",
                    l.trim()
                );
            }
            let found: usize = lines.iter().map(|l| calls_on_line(l, transport)).sum();
            let rostered = ROSTER
                .iter()
                .filter(|c| c.file == file && c.transport == *transport)
                .count();
            assert_eq!(
                found, rostered,
                "{file}: {found} production call(s) of `{transport}` but {rostered} on the usage \
                 roster. A new model call path must decide its usage record (#2902): emit one \
                 through `crate::usage` and add a `CallSite` with its behavioral test, or add an \
                 `Exempt` entry saying why it cannot."
            );
        }
    }
    // Every rostered caller really does contain its transport call.
    for c in ROSTER {
        let body = fn_body(&read_src(c.file), c.caller);
        assert!(
            body.contains(c.transport),
            "{}::{} no longer calls `{}`",
            c.file,
            c.caller,
            c.transport
        );
    }
}

#[test]
fn the_call_counter_sees_a_call_beside_a_signature_but_not_a_declaration() {
    assert_eq!(
        calls_on_line(
            "pub fn single_shot_chat(req: &R) -> X {",
            "single_shot_chat("
        ),
        0
    );
    assert_eq!(
        calls_on_line("fn f() -> X { single_shot_chat(&r) }", "single_shot_chat("),
        1
    );
    assert_eq!(
        calls_on_line("x(single_shot_chat_hosted(&r))", "single_shot_chat("),
        0
    );
}

#[test]
fn every_roster_path_calls_the_writer() {
    // The writer's entry points: the shared reply seam, the writer itself,
    // and the two per-path payload builders that delegate to it.
    const WRITER: &[&str] = &[
        "usage_payload(",
        "map_call_token_payload(",
        "turn_tokens_payload(",
    ];
    let mut sites: Vec<((&str, &str), &str)> = ROSTER
        .iter()
        .filter_map(|c| match c.duty {
            Duty::Emits { writer_in, test } => Some((writer_in, test)),
            Duty::Transport | Duty::Exempt(_) => None,
        })
        .collect();
    sites.push((
        CONTAINER_TURN,
        "dispatch_internal::tests::usage_conformance_container_turn",
    ));
    for ((file, func), test) in sites {
        let body = fn_body(&read_src(file), func);
        assert!(
            WRITER.iter().any(|w| body.contains(w)),
            "{file}::{func} must emit its usage record through `crate::usage` (behavioral test: {test})"
        );
    }
    for c in ROSTER {
        if let Duty::Exempt(why) = c.duty {
            assert!(
                !why.is_empty(),
                "{}::{} is exempt without a reason",
                c.file,
                c.caller
            );
        }
    }
}
