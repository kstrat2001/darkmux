//! (#1221) The prefill bodies and the operator's bounds.
//!
//! The invariant that matters is the last one: keep-thinking and wrap-up must
//! stay one delimiter apart. If that ever stops being true the design has
//! drifted, because that single character is the entire control surface.

use super::*;

#[test]
fn continue_prefill_leaves_the_think_block_open() {
    let p = continue_thinking_prefill("traced the write path, still checking");
    assert!(p.starts_with(THINK_OPEN));
    assert!(
        !p.contains("</think>"),
        "an OPEN block is what makes the model keep thinking rather than answer"
    );
    assert!(p.ends_with("still checking"), "the reasoning is handed back verbatim");
}

#[test]
fn conclude_prefill_closes_the_block() {
    let p = conclude_now_prefill("traced the write path");
    assert!(p.starts_with(THINK_OPEN));
    assert!(
        p.ends_with(THINK_CLOSE),
        "the CLOSING delimiter is what forces an answer conditioned on the reasoning"
    );
    assert!(p.contains("traced the write path"), "the work survives into the prefill");
}

#[test]
fn the_two_prefills_differ_only_by_the_closing_delimiter() {
    let r = "some reasoning";
    assert_eq!(
        conclude_now_prefill(r),
        format!("{}{}", continue_thinking_prefill(r), THINK_CLOSE),
        "keep-thinking and wrap-up are the same call one delimiter apart — \
         if this ever stops being true, the design has drifted"
    );
}

/// (#1221) The production path no longer calls `conclude_now_prefill`: it
/// writes `THINK_CLOSE` into the accumulation once, then keeps building every
/// later prefill with `continue_thinking_prefill`. That refactor is only safe
/// because the two are the same string, which is what this pins.
///
/// The equivalence is load-bearing rather than cosmetic — it is what lets a
/// concluded thought survive subsequent checkpoints without the harness having
/// to remember to re-append a delimiter it would eventually forget.
#[test]
fn closing_the_accumulation_equals_the_concluded_prefill() {
    let reasoning = "checked the roster, then the sink";
    let mut accumulated = reasoning.to_string();
    accumulated.push_str(THINK_CLOSE);

    assert_eq!(
        continue_thinking_prefill(&accumulated),
        conclude_now_prefill(reasoning),
        "writing THINK_CLOSE into the accumulation must produce exactly the \
         concluded prefill; if these diverge, a concluded thought silently \
         re-opens on the next checkpoint"
    );
}
