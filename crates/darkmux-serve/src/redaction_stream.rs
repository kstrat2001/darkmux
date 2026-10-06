//! The streaming half of [`crate::redaction::redact_reads`] (#3073): a remote
//! caller's response is redacted token by token as it passes.
//!
//! [`Redaction`] stays the ONE owner of what is redacted ([`redact_text`], one
//! rewrite per string). This module only changes the traversal: a JSON string
//! token, a value or an object key, is unescaped, rewritten and re-escaped; every
//! other byte (structure, whitespace, numbers, literals) passes through as it
//! came. There is no size cap and no depth cap: nothing recurses, so the
//! nesting is tracked by a bit stack (one bit per level) and valid JSON of any
//! depth stays in JSON mode.
//!
//! What is bounded: the buffer is the largest single string or bare token, and
//! the nesting state is one bit per level plus, per open object, a small record
//! and 8 bytes per distinct key it has emitted (so two keys that redact alike
//! both survive). The response itself is never held, so no body is too big.
//!
//! Fail closed. A JSON body whose bytes are not JSON (a bare word that is no
//! number or literal, an unterminated or badly escaped string, a mismatched
//! bracket, a string serde_json rejects such as a lone surrogate, or a body that
//! ends mid-value) ends the stream with [`Malformed`], a truncated body, rather
//! than being read as text: text redaction would see the JSON still escaped
//! (`\n/Users/x` reads as `n/Users/x`) and miss what its boundary matchers
//! need. Everything sent before the error was already rewritten token by token.
//! Text redaction ([`StreamRedactor::text`], line by line) is only for routes
//! declared non-JSON.

use crate::redaction::{redact_text, Redaction};
use std::borrow::Cow;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

/// The body claimed to be JSON and was not, so the stream stops (#3073).
#[derive(Debug)]
pub(crate) struct Malformed;

impl std::fmt::Display for Malformed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("response body is not valid JSON; withheld from a remote reader")
    }
}

impl std::error::Error for Malformed {}

/// One bit per open container: `true` for an object, `false` for an array.
#[derive(Default)]
struct KindStack {
    words: Vec<u64>,
    len: usize,
}

impl KindStack {
    fn push(&mut self, is_object: bool) {
        if self.len % 64 == 0 {
            self.words.push(0);
        }
        if is_object {
            self.words[self.len / 64] |= 1 << (self.len % 64);
        }
        self.len += 1;
    }

    fn last(&self) -> Option<bool> {
        let at = self.len.checked_sub(1)?;
        Some(self.words[at / 64] >> (at % 64) & 1 == 1)
    }

    fn pop(&mut self) {
        if self.len == 0 {
            return;
        }
        self.len -= 1;
        if self.len % 64 == 0 {
            self.words.pop();
        } else {
            self.words[self.len / 64] &= !(1 << (self.len % 64));
        }
    }
}

/// An open object. It remembers a hash of each key it has emitted, and whether
/// that key was rewritten, so two keys that redact alike both survive, the later
/// one suffixed (`name #2`).
struct Object {
    keys: HashMap<u64, bool>,
    expect_key: bool,
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
    kinds: KindStack,
    /// The open objects, innermost last: one entry per `true` bit in `kinds`.
    objects: Vec<Object>,
    /// The token (JSON mode) or line (text mode) being collected.
    pending: Vec<u8>,
    escaped: bool,
    #[cfg(test)]
    pub(crate) high_water: usize,
}

impl StreamRedactor {
    /// For a body that claims to be JSON.
    pub(crate) fn json(r: Arc<Redaction>) -> Self {
        Self { r, scan: Scan::Between, kinds: KindStack::default(), objects: Vec::new(), pending: Vec::new(), escaped: false, #[cfg(test)] high_water: 0 }
    }

    /// For a route declared non-JSON: redacted a line at a time.
    pub(crate) fn text(r: Arc<Redaction>) -> Self {
        Self { scan: Scan::Text, ..Self::json(r) }
    }

    /// Redact the next chunk of the body into `out`. A JSON body that turns out
    /// not to be JSON is an error; `out` then holds only what was already
    /// redacted.
    pub(crate) fn feed(&mut self, chunk: &[u8], out: &mut Vec<u8>) -> Result<(), Malformed> {
        let mut i = 0;
        while i < chunk.len() {
            i += match self.scan {
                Scan::Text => self.text_step(&chunk[i..], out),
                Scan::Str => self.string_step(&chunk[i..], out)?,
                Scan::Bare => self.bare_step(&chunk[i..], out)?,
                Scan::Between => self.between_step(chunk[i], out)?,
            };
            #[cfg(test)]
            {
                self.high_water = self.high_water.max(self.pending.len());
            }
        }
        Ok(())
    }

    /// The body ended: flush what is still pending. A JSON body must end
    /// between values with every container closed.
    pub(crate) fn finish(&mut self, out: &mut Vec<u8>) -> Result<(), Malformed> {
        match self.scan {
            Scan::Text => {
                self.flush_line(out);
                Ok(())
            }
            Scan::Bare if self.kinds.len == 0 && is_json_scalar(&self.pending) => {
                out.append(&mut self.pending);
                Ok(())
            }
            Scan::Between if self.kinds.len == 0 => Ok(()),
            _ => Err(Malformed),
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

    fn string_step(&mut self, rest: &[u8], out: &mut Vec<u8>) -> Result<usize, Malformed> {
        for (n, &b) in rest.iter().enumerate() {
            if self.escaped {
                self.escaped = false;
            } else if b == b'\\' {
                self.escaped = true;
            } else if b == b'"' {
                self.pending.extend_from_slice(&rest[..=n]);
                self.finish_string(out)?;
                return Ok(n + 1);
            }
        }
        self.pending.extend_from_slice(rest);
        Ok(rest.len())
    }

    fn bare_step(&mut self, rest: &[u8], out: &mut Vec<u8>) -> Result<usize, Malformed> {
        let n = rest.iter().position(|&b| is_delimiter(b)).unwrap_or(rest.len());
        self.pending.extend_from_slice(&rest[..n]);
        if n < rest.len() {
            if !is_json_scalar(&self.pending) {
                return Err(Malformed);
            }
            out.append(&mut self.pending);
            self.scan = Scan::Between;
        }
        Ok(n)
    }

    fn between_step(&mut self, b: u8, out: &mut Vec<u8>) -> Result<usize, Malformed> {
        match b {
            b' ' | b'\t' | b'\n' | b'\r' => out.push(b),
            b'"' => {
                self.scan = Scan::Str;
                self.pending.push(b);
            }
            b'{' | b'[' => self.open(b, out),
            b'}' | b']' => self.close(b, out)?,
            b',' | b':' => self.separator(b, out),
            _ => {
                self.scan = Scan::Bare;
                self.pending.push(b);
            }
        }
        Ok(1)
    }

    fn open(&mut self, b: u8, out: &mut Vec<u8>) {
        self.kinds.push(b == b'{');
        if b == b'{' {
            self.objects.push(Object { keys: HashMap::new(), expect_key: true });
        }
        out.push(b);
    }

    fn close(&mut self, b: u8, out: &mut Vec<u8>) -> Result<(), Malformed> {
        match (self.kinds.last(), b) {
            (Some(true), b'}') => {
                self.objects.pop();
            }
            (Some(false), b']') => {}
            _ => return Err(Malformed),
        }
        self.kinds.pop();
        out.push(b);
        Ok(())
    }

    fn separator(&mut self, b: u8, out: &mut Vec<u8>) {
        if self.kinds.last() == Some(true) {
            if let Some(o) = self.objects.last_mut() {
                o.expect_key = b == b',';
            }
        }
        out.push(b);
    }

    fn finish_string(&mut self, out: &mut Vec<u8>) -> Result<(), Malformed> {
        let raw = std::mem::take(&mut self.pending);
        let text = serde_json::from_slice::<String>(&raw).map_err(|_| Malformed)?;
        self.scan = Scan::Between;
        let redacted = redact_text(&text, &self.r);
        let changed = matches!(redacted, Cow::Owned(_));
        let key_slot = if self.kinds.last() == Some(true) { self.objects.last_mut().filter(|o| o.expect_key) } else { None };
        let name = match key_slot {
            Some(o) => unique_key(&mut o.keys, redacted.into_owned(), changed),
            None => redacted.into_owned(),
        };
        if name == text {
            out.extend_from_slice(&raw);
        } else {
            out.extend_from_slice(serde_json::to_string(&name).unwrap_or_default().as_bytes());
        }
        Ok(())
    }
}

fn key_hash(key: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    h.finish()
}

/// The key to emit: `name`, or `name #2`, `name #3` ... when an earlier key of
/// this object already took it and either one was rewritten. A duplicate the
/// caller sent (neither rewritten) passes as sent. `keys` maps the hash of an
/// emitted key to whether it was rewritten; a 64-bit collision at worst adds a
/// spurious suffix.
fn unique_key(keys: &mut HashMap<u64, bool>, name: String, changed: bool) -> String {
    let mut key = name.clone();
    let mut n = 2;
    while keys.get(&key_hash(&key)).is_some_and(|&earlier_changed| earlier_changed || changed) {
        key = format!("{name} #{n}");
        n += 1;
    }
    keys.insert(key_hash(&key), changed);
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
    fn try_stream(body: &[u8], size: usize, json_mode: bool) -> (Vec<u8>, Result<(), Malformed>) {
        let mut s = if json_mode { StreamRedactor::json(rules()) } else { StreamRedactor::text(rules()) };
        let mut out = Vec::new();
        for c in body.chunks(size) {
            if let Err(e) = s.feed(c, &mut out) {
                return (out, Err(e));
            }
        }
        let end = s.finish(&mut out);
        (out, end)
    }

    /// A body that must be well formed: leaving JSON mode is an error, which is what makes an
    /// escape-tracking mutant visible (it used to fall back to text and still parse equal).
    fn stream(body: &[u8], size: usize, json_mode: bool) -> Vec<u8> {
        let (out, end) = try_stream(body, size, json_mode);
        end.unwrap_or_else(|e| panic!("{e}: left JSON mode on {}", String::from_utf8_lossy(body)));
        out
    }

    fn fixtures() -> Vec<String> {
        vec![
            json!({"peer 100.64.7.7": "x", "ok": {"/Users/someone/k": ["at 10.0.0.1", 1, -2.5e3, true, false, null]}}).to_string(),
            json!([{"a": "from 100.64.7.7"}, [], {}, [[["/home/someone/deep"]]], "plain", 42]).to_string(),
            r#"{"esc":"quote \" slash \\ solidus \/ nl \n tab \t uni é pair 😀 at 100.64.7.7","k\"q 10.0.0.1":"v"}"#.to_string(),
            "  {\n \"a\" : [ 1 , 2 ] ,\n \"b\":\"é ü 日本語 100.64.7.7\" }  ".to_string(),
            r#""just a string 100.64.7.7""#.to_string(),
            r#"{"esc":"err\n/Users/someone/x\tat\t100.64.7.7 \\n10.0.0.1 \" 100.64.7.7","\n100.64.7.7":["\t/home/someone/y"]}"#.to_string(),
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
    fn malformed_json_ends_the_stream_and_leaks_nothing() {
        for body in [
            format!(r#"{{"a":"{IP}", {IP} oops}}"#),
            format!(r#"{{"a":"x"] at {IP}"#),
            format!(r#"["fine", "bad \q escape {IP}"]"#),
            format!(r#"{{"a":"unterminated {IP}"#),
            format!("not json at all, {IP} in /Users/someone/x\nsecond line {IP}"),
            format!(r#"{{"a":1}} trailing {IP}"#),
            format!(r#"{{"a":"ok"}}{IP}"#),
            format!(r#"{{"a":"{IP}""#),
            format!(r#"["\ud800","\/Users\/kfake\/x \n{IP}"]"#),
        ] {
            for size in [1, 5, body.len()] {
                let (out, end) = try_stream(body.as_bytes(), size, true);
                let out = String::from_utf8(out).unwrap();
                assert!(end.is_err(), "chunk {size}: not an error: {body}");
                assert!(!out.contains(IP) && !out.contains("/Users/") && !out.contains("kfake"), "chunk {size}: leaked in {out}");
            }
        }
        let out = String::from_utf8(stream(format!("failed at {IP}\nnext {IP}").as_bytes(), 3, false)).unwrap();
        assert_eq!(out, format!("failed at {ADDRESS_HIDDEN}\nnext {ADDRESS_HIDDEN}"));
    }

    #[test]
    fn nesting_of_any_depth_stays_json_with_one_bit_per_level() {
        let depth = 200_000;
        let body = format!("{}\"err\\n/Users/someone/x\\t{IP}\"{}", "[".repeat(depth), "]".repeat(depth));
        let mut s = StreamRedactor::json(rules());
        let mut out = Vec::new();
        for c in body.as_bytes().chunks(4096) {
            s.feed(c, &mut out).unwrap();
        }
        s.finish(&mut out).unwrap();
        assert!(s.kinds.words.is_empty(), "closed again");
        let out = String::from_utf8(out).unwrap();
        assert!(!out.contains(IP) && !out.contains("/Users/") && out.starts_with("[[[") && out.ends_with("]]]"));
    }

    #[test]
    fn a_mismatched_bracket_is_an_error() {
        for body in [r#"["a"}"#, r#"{"a":1]"#, "]", r#"{"a":bad}"#] {
            let mut s = StreamRedactor::json(rules());
            assert!(s.feed(body.as_bytes(), &mut Vec::new()).and_then(|()| s.finish(&mut Vec::new())).is_err(), "{body}");
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
        s.feed(b"[", &mut out).unwrap();
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
            s.feed(&chunk, &mut out).unwrap();
            assert!(!String::from_utf8_lossy(&out).contains(IP));
        }
        out.clear();
        s.feed(b"]", &mut out).unwrap();
        s.finish(&mut out).unwrap();
        assert!(sent >= 100_000_000);
        assert!(s.high_water <= record.len(), "peak buffer {} over one record {}", s.high_water, record.len());
    }
}
