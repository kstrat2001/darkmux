//! Test counts from a JUnit XML report: the one test-results format the
//! grader reads, so a fixture's runner needs no code here, only a reporter
//! that writes it. Counted, not fully parsed: one `<testcase>` per test, and
//! a `<failure>`, `<error>` or `<skipped>` inside it says how it ended.
//! Comments and CDATA are dropped first, so a test's own output or a
//! summary comment can never add a testcase.

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Counts {
    pub tests: u64,
    pub failed: u64,
    pub skipped: u64,
    /// Each passing testcase's `name`, as the report spells it.
    pub passing: Vec<String>,
}

impl Counts {
    pub fn passed(&self) -> u64 {
        self.tests - self.failed - self.skipped
    }
}

/// Count the report's testcases. `None` when the text holds no
/// `<testsuites>`/`<testsuite>` element, so it is not a JUnit report.
pub(crate) fn parse(xml: &str) -> Option<Counts> {
    let xml = strip(strip(xml, "<!--", "-->").as_str(), "<![CDATA[", "]]>");
    if !(has_tag(&xml, "testsuites") || has_tag(&xml, "testsuite")) {
        return None;
    }
    let mut counts = Counts::default();
    let mut rest = xml.as_str();
    while let Some(start) = find_tag(rest, "testcase") {
        let after = &rest[start..];
        let end = tag_end(after)?;
        let tag = &after[..end];
        counts.tests += 1;
        let self_closing = tag.trim_end_matches('>').trim_end().ends_with('/');
        let (failed, skipped, next) = if self_closing {
            (false, false, end)
        } else {
            let body_end = after.find("</testcase>")?;
            let body = &after[end..body_end];
            (
                has_tag(body, "failure") || has_tag(body, "error"),
                has_tag(body, "skipped"),
                body_end,
            )
        };
        if failed {
            counts.failed += 1;
        } else if skipped {
            counts.skipped += 1;
        } else {
            counts
                .passing
                .push(attribute(tag, "name").unwrap_or_default());
        }
        rest = &after[next..];
    }
    Some(counts)
}

/// A quoted attribute's raw value in one tag.
fn attribute(tag: &str, name: &str) -> Option<String> {
    let mut from = 0;
    while let Some(i) = tag[from..].find(name) {
        let at = from + i;
        let before = tag[..at].chars().last();
        let rest = tag[at + name.len()..].trim_start();
        if before.is_some_and(char::is_whitespace) {
            if let Some(rest) = rest.strip_prefix('=') {
                let rest = rest.trim_start();
                let quote = rest.chars().next()?;
                if quote == '"' || quote == '\'' {
                    let value = &rest[1..];
                    return value.find(quote).map(|e| value[..e].to_string());
                }
            }
        }
        from = at + name.len();
    }
    None
}

/// Remove every `open … close` span.
fn strip(text: &str, open: &str, close: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(open) {
        out.push_str(&rest[..i]);
        match rest[i..].find(close) {
            Some(j) => rest = &rest[i + j + close.len()..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// The byte offset of the next `<name` opening tag (followed by whitespace,
/// `/` or `>`, so `<testcases` is not `<testcase`).
fn find_tag(text: &str, name: &str) -> Option<usize> {
    let needle = format!("<{name}");
    let mut from = 0;
    while let Some(i) = text[from..].find(&needle) {
        let at = from + i;
        match text[at + needle.len()..].chars().next() {
            Some(c) if c.is_whitespace() || c == '/' || c == '>' => return Some(at),
            None => return None,
            _ => from = at + needle.len(),
        }
    }
    None
}

fn has_tag(text: &str, name: &str) -> bool {
    find_tag(text, name).is_some()
}

/// The offset just past the `>` that closes the tag starting at 0, skipping
/// any `>` inside a quoted attribute value.
fn tag_end(text: &str) -> Option<usize> {
    let mut quote = None;
    for (i, c) in text.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, '>') => return Some(i + 1),
            _ => {}
        }
    }
    None
}
