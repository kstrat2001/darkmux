use super::*;

#[test]
fn a_muse_glimmer_reply_with_its_channel_markup_is_named() {
    // The reply the #316 gate got from mlx_lm.server 0.32.1.
    let reply = "to=self<|message|>The file has 17 and 25. Sum = 42.<|eom|><|start|>assistant to=user<|message|>42";
    let leak = leaked(reply).expect("a leak");
    assert_eq!(leak.format, Format::MuseGlimmer);
    assert_eq!(leak.marker, "to=self<|message|>");
}

#[test]
fn a_muse_glimmer_turn_opener_is_named_after_leading_whitespace() {
    let leak = leaked("\n  <|start|>assistant to=self<|message|>thinking").expect("a leak");
    assert_eq!(leak.format, Format::MuseGlimmer);
}

#[test]
fn a_harmony_reply_with_its_channel_markup_is_named() {
    for reply in [
        "<|channel|>analysis<|message|>We need to add.<|end|><|start|>assistant<|channel|>final<|message|>42",
        "<|start|>assistant<|channel|>analysis<|message|>We need to add.",
    ] {
        let leak = leaked(reply).expect("a leak");
        assert_eq!(leak.format, Format::Harmony, "{reply}");
        assert_eq!(leak.marker, "<|channel|>");
    }
}

#[test]
fn a_parsed_reply_is_not_a_leak() {
    for reply in ["42", "", "\n\n42", "{\"findings\":[]}", "to=self is how the model addresses itself"] {
        assert!(leaked(reply).is_none(), "{reply:?}");
    }
}

#[test]
fn markup_quoted_after_the_opening_is_not_a_leak() {
    // A reviewer quoting a parser's source, or an answer explaining the
    // format: the markers are content, not an unparsed channel.
    for reply in [
        "The parser matches `<|channel|>` at the start of a reply.",
        "A Muse Glimmer reply looks like to=self<|message|>... when unparsed.",
        "to=\n<|message|>",
    ] {
        assert!(leaked(reply).is_none(), "{reply:?}");
    }
}

#[test]
fn the_warning_names_the_model_the_marker_and_engines_that_parse_it() {
    let leak = leaked("to=self<|message|>x").unwrap();
    let w = warning(&leak, "darkmux:muse-glimmer-30b");
    assert!(w.contains("darkmux:muse-glimmer-30b"), "{w}");
    assert!(w.contains("`to=self<|message|>`"), "{w}");
    assert!(w.contains("llama-server b10353"), "{w}");
    assert!(w.contains("muse_glimmer"), "{w}");
    assert!(w.contains("as the model wrote it"), "{w}");
    let h = warning(&leaked("<|channel|>analysis<|message|>x").unwrap(), "gpt-oss-120b");
    assert!(h.contains("Harmony"), "{h}");
    assert!(h.contains("llama-server"), "{h}");
}

#[test]
fn a_recipient_must_be_a_short_name() {
    // No name, or prose running far past a recipient's length, before a
    // `<|message|>`: not a channel header.
    let long = format!("to={}<|message|>x", "a".repeat(80));
    for reply in ["to=<|message|>x", long.as_str()] {
        assert!(leaked(reply).is_none(), "{reply:?}");
    }
    let tool = leaked("to=functions.read_file<|message|>{}").expect("a tool recipient");
    assert_eq!(tool.marker, "to=functions.read_file<|message|>");
}
