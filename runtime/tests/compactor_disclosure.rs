//! (Second review round, MUST FIX 1 follow-up: "neither disclosure call
//! site is test-covered") Pins the RUNTIME-side half of the unset-compactor
//! disclosure (`compaction::compactor_disclosure_message`, wired at its call
//! site in `main.rs`'s `run_dispatch`) by spawning the REAL `darkmux-runtime`
//! binary — the same `CARGO_BIN_EXE_darkmux-runtime` seam
//! `resume_refusal.rs` already uses, and the same technique the reviewer
//! used to confirm the disclosure live. Before this, `compactor_disclosure_
//! message` itself was unit-tested but nothing proved `main.rs` actually
//! calls it and prints the result BEFORE any network attempt.
//!
//! No LMStudio, no mock server: `--base-url http://127.0.0.1:1` points at a
//! guaranteed-dead port (port 1 is a reserved low port nothing binds), so
//! the eventual chat-completions call fails fast with a connection error —
//! but only AFTER the disclosure has already printed, since
//! `validate_compaction_cli_inputs` → `compactor_disclosure_message` both
//! run before the runtime ever dials out.

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_darkmux-runtime")
}

/// The MUST FIX 1 (second review round) case itself: threshold-only mode
/// (an absolute `--compact-threshold-tokens`, no `--context-window`, no
/// `--compactor-model`) is the gap the widened gate exists to close — the
/// reviewer's own live run of the pre-fix binary produced no message here.
#[test]
fn threshold_only_mode_discloses_the_unset_compactor() {
    let output = Command::new(bin())
        .args([
            "run",
            "--model",
            "darkmux:does-not-need-to-exist",
            "--system",
            "You are a test.",
            "--prompt",
            "hello",
            "--compact-threshold-tokens",
            "30000",
            "--base-url",
            "http://127.0.0.1:1",
            "--no-stream",
        ])
        .output()
        .expect("spawning darkmux-runtime");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no compactor is configured for this dispatch"),
        "threshold-only mode (no --context-window) must still disclose the unset compactor — \
         this is exactly the MUST FIX 1 (second review round) gap: stderr was: {stderr}"
    );
    assert!(
        stderr.to_ascii_lowercase().contains("compaction is off"),
        "disclosure must say plainly that compaction is off: {stderr}"
    );
    assert!(
        stderr.contains("30000"),
        "disclosure must name the actual threshold when no window is known: {stderr}"
    );
}

/// The original (first review round) case: a context window with no
/// compactor bound. Kept alongside the threshold-only case so a future
/// regression that narrows the gate back to "context_window only" is caught
/// by the sibling test even if this one is edited in isolation.
#[test]
fn context_window_mode_discloses_the_unset_compactor() {
    let output = Command::new(bin())
        .args([
            "run",
            "--model",
            "darkmux:does-not-need-to-exist",
            "--system",
            "You are a test.",
            "--prompt",
            "hello",
            "--context-window",
            "101000",
            "--base-url",
            "http://127.0.0.1:1",
            "--no-stream",
        ])
        .output()
        .expect("spawning darkmux-runtime");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no compactor is configured for this dispatch"),
        "a real context window with no compactor bound must disclose: stderr was: {stderr}"
    );
    assert!(
        stderr.contains("101000"),
        "disclosure must name the actual context window: {stderr}"
    );
}

/// Negative control: a bound compactor must stay silent on both triggers —
/// proves the disclosure is genuinely conditioned on `compactor_model`, not
/// just always printing whenever a trigger is present.
#[test]
fn bound_compactor_stays_silent() {
    let output = Command::new(bin())
        .args([
            "run",
            "--model",
            "darkmux:does-not-need-to-exist",
            "--system",
            "You are a test.",
            "--prompt",
            "hello",
            "--compact-threshold-tokens",
            "30000",
            "--compactor-model",
            "darkmux:qwen3-4b-instruct-2507",
            "--base-url",
            "http://127.0.0.1:1",
            "--no-stream",
        ])
        .output()
        .expect("spawning darkmux-runtime");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("no compactor is configured for this dispatch"),
        "a bound compactor must never trigger the unset-compactor disclosure: stderr was: \
         {stderr}"
    );
    // (Third review round) Without this, a mangled/renamed flag (e.g.
    // `--compact-threshold-tokens` typo'd) fails argument parsing before the
    // disclosure logic is ever reached, so the assertion above passes
    // vacuously — proven: mangling the flag name above yields `unknown
    // flag`, zero matches for the disclosure substring either way, exit
    // code 2. Require the run to have actually reached the transport
    // (dead-port connection failure, exit code 1) so a parse failure can no
    // longer masquerade as a passing negative control.
    assert_eq!(
        output.status.code(),
        Some(1),
        "the run must reach the transport (a dead-port connection failure) for the silence \
         above to mean anything — a non-1 exit means argument parsing itself failed before the \
         disclosure was ever reachable: stderr was: {stderr}"
    );
}
