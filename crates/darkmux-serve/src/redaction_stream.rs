//! The streaming half of [`crate::redaction::redact_reads`] (#3073): a remote
//! caller's response is redacted token by token as it passes, so memory is
//! bounded by the largest single value, not by the response.
//!
//! [`Redaction`] stays the ONE owner of what is redacted ([`redact_text`], one
//! rewrite per string). This module only changes the traversal: a JSON string
//! token, a value or an object key, is unescaped, rewritten and re-escaped; every
//! other byte (structure, whitespace, numbers, literals) passes through as it
//! came. There is no size cap. The one bound is
//! [`MAX_DEPTH`], serde_json's own recursion limit (not a guess), so a nesting
//! bomb costs a bounded stack. Memory is otherwise the largest single string,
//! bare token or, once a body has failed over to text, line: for well-formed
//! JSON that is one value; a body that is garbage with no newline is held whole
//! (1x its size, where the parse held ~6.6x).
//!
//! Fail closed: bytes that are not JSON (a bare word that is no number or
//! literal, an unterminated or badly escaped string, a mismatched bracket, a
//! body past [`MAX_DEPTH`], or any body on a non-JSON route) switch the rest of
//! the body to line-by-line text redaction ([`redact_text`]), so nothing
//! unredacted is ever emitted. Everything sent before the switch was already
//! rewritten token by token.

use crate::redaction::{redact_text, Redaction};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

/// serde_json's recursion limit.
const MAX_DEPTH: usize = 128;

/// An open `{` or `[`. An object remembers the keys it has emitted so two keys
/// that redact alike both survive, the later one suffixed (`name #2`).
enum Frame {
    Array,
    Object { keys: HashMap<String, bool>, expect_key: bool },
}

#[derive(Clone, Copy, PartialEq)]
enum Scan {
    Between,
    Str,
    Bare,
    Text,
}

pub(crate) struct StreamRedactor {
    r: Arc<Redaction>,
    scan: Scan,
    stack: Vec<Frame>,
    /// The token (JSON mode) or line (text mode) being collected.
    pending: Vec<u8>,
    escaped: bool,
    #[cfg(test)]
    pub(crate) high_water: usize,
}

impl StreamRedactor {
    /// For a body that claims to be JSON.
    pub(crate) fn json(r: Arc<Redaction>) -> Self {
        Self { r, scan: Scan::Between, stack: Vec::new(), pending: Vec::new(), escaped: false, #[cfg(test)] high_water: 0 }
    }

    /// For any other body: redacted a line at a time.
    pub(crate) fn text(r: Arc<Redaction>) -> Self {
        Self { scan: Scan::Text, ..Self::json(r) }
    }

    /// Redact the next chunk of the body into `out`.
    pub(crate) fn feed(&mut self, chunk: &[u8], out: &mut Vec<u8>) {
        let mut i = 0;
        while i < chunk.len() {
            i += match self.scan {
                Scan::Text => self.text_step(&chunk[i..], out),
                Scan::Str => self.string_step(&chunk[i..], out),
                Scan::Bare => self.bare_step(&chunk[i..], out),
                Scan::Between => self.between_step(chunk[i], out),
            };
            #[cfg(test)]
            {
                self.high_water = self.high_water.max(self.pending.len());
            }
        }
    }

    /// The body ended: flush what is still pending.
    pub(crate) fn finish(&mut self, out: &mut Vec<u8>) {
        if self.scan == Scan::Bare && is_json_scalar(&self.pending) {
            out.append(&mut self.pending);
        } else if self.scan != Scan::Between {
            self.scan = Scan::Text;
            self.flush_line(out);
        }
    }

    fn text_step(&mut self, rest: &[u8], out: &mut Vec<u8>) -> usize {
        match rest.iter().position(|&b| b == b'\n') {
            Some(n) => {
                self.pending.extend_from_slice(&rest[..=n]);
                self.flush_line(out);
                n + 1
            }
            None => {
                self.pending.extend_from_slice(rest);
                rest.len()
            }
        }
    }

    fn flush_line(&mut self, out: &mut Vec<u8>) {
        let line = String::from_utf8_lossy(&self.pending);
        out.extend_from_slice(redact_text(&line, &self.r).as_bytes());
        self.pending.clear();
    }

    /// Give up on JSON: `bytes` and what is pending are redacted as text.
    fn fail(&mut self, bytes: &[u8]) -> usize {
        self.scan = Scan::Text;
        self.stack.clear();
        self.pending.extend_from_slice(bytes);
        bytes.len()
    }

    fn string_step(&mut self, rest: &[u8], out: &mut Vec<u8>) -> usize {
        for (n, &b) in rest.iter().enumerate() {
            if self.escaped {
                self.escaped = false;
            } else if b == b'\\' {
                self.escaped = true;
            } else if b == b'"' {
                self.pending.extend_from_slice(&rest[..=n]);
                self.finish_string(out);
                return n + 1;
            }
        }
        self.pending.extend_from_slice(rest);
        rest.len()
    }

    fn bare_step(&mut self, rest: &[u8], out: &mut Vec<u8>) -> usize {
        let n = rest.iter().position(|&b| is_delimiter(b)).unwrap_or(rest.len());
        self.pending.extend_from_slice(&rest[..n]);
        if n < rest.len() && !is_json_scalar(&self.pending) {
            return self.fail(&[]) + n;
        }
        if n < rest.len() {
            out.append(&mut self.pending);
            self.scan = Scan::Between;
        }
        n
    }

    fn between_step(&mut self, b: u8, out: &mut Vec<u8>) -> usize {
        match b {
            b' ' | b'\t' | b'\n' | b'\r' => out.push(b),
            b'"' => {
                self.scan = Scan::Str;
                self.pending.push(b);
            }
            b'{' | b'[' => return self.open(b, out),
            b'}' | b']' => return self.close(b, out),
            b',' | b':' => self.separator(b, out),
            _ => {
                self.scan = Scan::Bare;
                self.pending.push(b);
            }
        }
        1
    }

    fn open(&mut self, b: u8, out: &mut Vec<u8>) -> usize {
        if self.stack.len() >= MAX_DEPTH {
            return self.fail(&[b]);
        }
        self.stack.push(if b == b'{' { Frame::Object { keys: HashMap::new(), expect_key: true } } else { Frame::Array });
        out.push(b);
        1
    }

    fn close(&mut self, b: u8, out: &mut Vec<u8>) -> usize {
        match (self.stack.last(), b) {
            (Some(Frame::Object { .. }), b'}') | (Some(Frame::Array), b']') => {
                self.stack.pop();
                out.push(b);
                1
            }
            _ => self.fail(&[b]),
        }
    }

    fn separator(&mut self, b: u8, out: &mut Vec<u8>) {
        if let Some(Frame::Object { expect_key, .. }) = self.stack.last_mut() {
            *expect_key = b == b',';
        }
        out.push(b);
    }

    fn finish_string(&mut self, out: &mut Vec<u8>) {
        let raw = std::mem::take(&mut self.pending);
        let Ok(text) = serde_json::from_slice::<String>(&raw) else {
            self.pending = raw;
            self.fail(&[]);
            return;
        };
        self.scan = Scan::Between;
        let redacted = redact_text(&text, &self.r);
        let changed = matches!(redacted, Cow::Owned(_));
        let name = match self.stack.last_mut() {
            Some(Frame::Object { keys, expect_key: true }) => unique_key(keys, redacted.into_owned(), changed),
            _ => redacted.into_owned(),
        };
        if name == text {
            out.extend_from_slice(&raw);
        } else {
            out.extend_from_slice(serde_json::to_string(&name).unwrap_or_default().as_bytes());
        }
    }
}

/// The key to emit: `name`, or `name #2`, `name #3` ... when an earlier key of
/// this object already took it and either one was rewritten. A duplicate the
/// caller sent (neither rewritten) passes as sent. `keys` maps an emitted key to
/// whether it was rewritten.
fn unique_key(keys: &mut HashMap<String, bool>, name: String, changed: bool) -> String {
    let mut key = name.clone();
    let mut n = 2;
    while keys.get(&key).is_some_and(|&earlier_changed| earlier_changed || changed) {
        key = format!("{name} #{n}");
        n += 1;
    }
    keys.insert(key.clone(), changed);
    key
}

fn is_delimiter(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b'"' | b'{' | b'}' | b'[' | b']' | b',' | b':')
}

/// A bare JSON token: a number, `true`, `false` or `null`. The grammar is
/// serde_json's, so there is no second one to keep in step.
fn is_json_scalar(tok: &[u8]) -> bool {
    !tok.is_empty() && serde_json::from_slice::<serde_json::Value>(tok).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redaction::ADDRESS_HIDDEN;
    use serde_json::{json, Value};

    const IP: &str = "100.64.7.7";

    fn rules() -> Arc<Redaction> {
        Arc::new(Redaction::from_parts(&[], &[], None, None))
    }

    /// The pre-streaming walk: parse the whole body, redact every key and value in place.
    fn oracle(r: &Redaction, v: &mut Value) {
        match v {
            Value::String(s) => {
                if let std::borrow::Cow::Owned(n) = crate::redaction::redact_text(s, r) {
                    *s = n;
                }
            }
            Value::Array(a) => a.iter_mut().for_each(|x| oracle(r, x)),
            Value::Object(o) => {
                o.values_mut().for_each(|x| oracle(r, x));
                let renames: Vec<(String, String)> = o
                    .keys()
                    .filter_map(|k| match crate::redaction::redact_text(k, r) {
                        std::borrow::Cow::Owned(n) => Some((k.clone(), n)),
                        std::borrow::Cow::Borrowed(_) => None,
                    })
                    .collect();
                for (old, new) in renames {
                    if let Some(val) = o.remove(&old) {
                        let mut key = new.clone();
                        let mut n = 2;
                        while o.contains_key(&key) {
                            key = format!("{new} #{n}");
                            n += 1;
                        }
                        o.insert(key, val);
                    }
                }
            }
            _ => {}
        }
    }

    /// Run a body through the streaming redactor in chunks of `size` bytes.
    fn stream(body: &[u8], size: usize, json_mode: bool) -> Vec<u8> {
        let mut s = if json_mode { StreamRedactor::json(rules()) } else { StreamRedactor::text(rules()) };
        let mut out = Vec::new();
        for c in body.chunks(size) {
            s.feed(c, &mut out);
        }
        s.finish(&mut out);
        out
    }

    fn fixtures() -> Vec<String> {
        vec![
            json!({"peer 100.64.7.7": "x", "ok": {"/Users/someone/k": ["at 10.0.0.1", 1, -2.5e3, true, false, null]}}).to_string(),
            json!([{"a": "from 100.64.7.7"}, [], {}, [[["/home/someone/deep"]]], "plain", 42]).to_string(),
            r#"{"esc":"quote \" slash \\ solidus \/ nl \n tab \t uni é pair 😀 at 100.64.7.7","k\"q 10.0.0.1":"v"}"#.to_string(),
            "  {\n \"a\" : [ 1 , 2 ] ,\n \"b\":\"é ü 日本語 100.64.7.7\" }  ".to_string(),
            r#""just a string 100.64.7.7""#.to_string(),
            "12345".to_string(),
            r#"{"n":[0,-0,1.5,1e10,1E-2,123456789012345678901234567890]}"#.to_string(),
        ]
    }

    #[test]
    fn streamed_output_equals_the_parse_and_walk_output_at_every_chunking() {
        let r = rules();
        for body in fixtures() {
            let mut want: Value = serde_json::from_str(&body).unwrap();
            oracle(&r, &mut want);
            for size in [1, 2, 3, 7, 64, body.len().max(1)] {
                let out = stream(body.as_bytes(), size, true);
                let got: Value = serde_json::from_slice(&out).unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out)));
                assert_eq!(got, want, "chunk {size}: {body}");
            }
        }
    }

    #[test]
    fn numbers_and_structure_pass_through_byte_for_byte() {
        let body = r#" {"n" : [0,-0,1.5,1e10,1E-2] , "t":true,"z":null} "#;
        assert_eq!(stream(body.as_bytes(), 3, true), body.as_bytes());
    }

    #[test]
    fn keys_that_redact_alike_all_survive() {
        let body = json!({"100.64.7.7": 1, "10.0.0.1": 2, "(address hidden)": 3, "ok": 4}).to_string();
        let out: Value = serde_json::from_slice(&stream(body.as_bytes(), 5, true)).unwrap();
        let o = out.as_object().unwrap();
        let mut got: Vec<i64> = o.values().map(|x| x.as_i64().unwrap()).collect();
        got.sort();
        assert_eq!(got, [1, 2, 3, 4], "{out}");
        assert!(o.contains_key(ADDRESS_HIDDEN) && o.contains_key(&format!("{ADDRESS_HIDDEN} #2")) && o.contains_key(&format!("{ADDRESS_HIDDEN} #3")), "{out}");
        let again: Value = serde_json::from_slice(&stream(br#"{"a":1,"a":2}"#, 4, true)).unwrap();
        assert_eq!(again, json!({"a": 2}), "a duplicate the caller sent stays a duplicate");
    }

    #[test]
    fn malformed_json_leaks_nothing_and_falls_back_to_text_redaction() {
        for body in [
            format!(r#"{{"a":"{IP}", {IP} oops}}"#),
            format!(r#"{{"a":"x"] at {IP}"#),
            format!(r#"["fine", "bad \q escape {IP}"]"#),
            format!(r#"{{"a":"unterminated {IP}"#),
            format!("not json at all, {IP} in /Users/someone/x\nsecond line {IP}"),
            format!(r#"{{"a":1}} trailing {IP}"#),
            format!(r#"{{"a":"ok"}}{IP}"#),
        ] {
            for size in [1, 5, body.len()] {
                let out = String::from_utf8(stream(body.as_bytes(), size, true)).unwrap();
                assert!(!out.contains(IP) && !out.contains("/Users/"), "chunk {size}: leaked in {out}");
            }
        }
        let out = String::from_utf8(stream(format!("failed at {IP}\nnext {IP}").as_bytes(), 3, false)).unwrap();
        assert_eq!(out, format!("failed at {ADDRESS_HIDDEN}\nnext {ADDRESS_HIDDEN}"));
    }

    #[test]
    fn nesting_past_the_depth_bound_falls_back_to_text_without_a_stack_that_grows() {
        let body = format!("{}\"{IP}\"{}", "[".repeat(200_000), "]".repeat(200_000));
        let mut s = StreamRedactor::json(rules());
        let mut out = Vec::new();
        for c in body.as_bytes().chunks(4096) {
            s.feed(c, &mut out);
            assert!(s.stack.len() <= MAX_DEPTH, "stack {}", s.stack.len());
        }
        s.finish(&mut out);
        assert!(!String::from_utf8(out).unwrap().contains(IP));
    }

    #[test]
    fn a_mismatched_bracket_gives_up_on_json() {
        for body in [r#"["a"}"#, r#"{"a":1]"#, "]", r#"{"a":bad}"#] {
            let mut s = StreamRedactor::json(rules());
            s.feed(body.as_bytes(), &mut Vec::new());
            assert!(s.scan == Scan::Text, "{body}");
        }
    }

    /// 100 MB of flow-shaped records stream through with no buffer past the largest record.
    #[test]
    fn a_hundred_megabyte_body_streams_with_buffers_bounded_by_one_record() {
        let record = json!({"ts": "2026-10-06T00:00:00Z", "cwd": "/Users/someone/work", "peer": IP, "note": "x".repeat(1400)}).to_string();
        let mut s = StreamRedactor::json(rules());
        let mut out = Vec::new();
        let mut sent = 0usize;
        let mut chunk = Vec::new();
        let mut first = true;
        s.feed(b"[", &mut out);
        while sent < 100_000_000 {
            chunk.clear();
            while chunk.len() < 65_536 {
                if !first {
                    chunk.push(b',');
                }
                first = false;
                chunk.extend_from_slice(record.as_bytes());
            }
            sent += chunk.len();
            out.clear();
            s.feed(&chunk, &mut out);
            assert!(!String::from_utf8_lossy(&out).contains(IP));
        }
        out.clear();
        s.feed(b"]", &mut out);
        s.finish(&mut out);
        assert!(sent >= 100_000_000);
        assert!(s.high_water <= record.len(), "peak buffer {} over one record {}", s.high_water, record.len());
    }
}
