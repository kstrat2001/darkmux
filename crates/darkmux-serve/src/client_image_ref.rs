//! Where a site uses an image reference (#3545): the places an added
//! image's `replace` may point at the new image.
//!
//! A reference is found where its text matches ([`crate::client_edit`]'s
//! rendered match, so `&amp;` and line breaks read as a page reads them),
//! and only where the file's type uses a URL there:
//!
//! - HTML, Vue, Svelte, Astro, Pug: the whole value of a URL attribute
//!   ([`URL_ATTRIBUTES`]: `src`, `href`, `poster`, `content`, ...), quoted, as a
//!   JSX/Svelte expression (`src={"/x"}`) or a Vue binding (`:src="'/x'"`);
//!   one candidate of a `srcset`; a string literal inside `<script>`.
//! - JavaScript and TypeScript: a whole string literal (template literals
//!   too), except the value of a text key or attribute ([`TEXT_KEYS`]: `alt`,
//!   `title`, ...).
//! - Markdown: an image or link target (`![a](x "t")`, `![a](<x>)`), a
//!   reference definition (`[h]: x`), a value in its front matter (YAML or
//!   TOML), an HTML attribute as above, a literal on an MDX `import` line.
//! - YAML, TOML, JSON: a whole value, except a text key's.
//! - Every one of those, and CSS: inside `url(...)`, quoted or not
//!   (Tailwind's `bg-[url(...)]` and CSS-in-JS included).
//!
//! Never words in a page's text, alt text, code or a comment ([`Page::skipped`]:
//! a Markdown code fence or span, an HTML comment, a JavaScript or CSS
//! comment), or a repository's own documents ([`outside_the_site`]). A query
//! or fragment after the reference (`/x.jpg?v=2`) stays as it was.
//!
//! What a check needs from the file as a whole (where its lines, quotes,
//! scripts, front matter and skipped ranges are) is found once per file
//! ([`Page`]); each candidate then looks only at what is next to it, so
//! matching takes time in proportion to the file, however many candidates
//! it holds.

use std::cell::OnceCell;

/// How a file of this type can use a URL.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Markup,
    Code,
    Style,
    Markdown,
    Yaml,
    Toml,
    Json,
}

/// The attributes whose whole value is a URL (a Vue binding's `:` or
/// `v-bind:` aside).
pub const URL_ATTRIBUTES: &[&str] = &[
    "src", "href", "poster", "content", "data-src", "data-href", "data-bg", "data-background",
    "data-original", "background", "xlink:href", "icon", "srcset", "data-srcset", "imagesrcset",
];

/// The attributes whose value is a list of `url descriptor` candidates.
const SRCSET_ATTRIBUTES: &[&str] = &["srcset", "data-srcset", "imagesrcset"];

/// Keys (and attributes) whose value is words, never a URL.
pub const TEXT_KEYS: &[&str] = &[
    "alt", "title", "caption", "label", "description", "aria-label", "placeholder", "text", "summary",
];

/// The longest attribute or key name looked for: a longer run of name
/// characters is not one, and is not read to its start.
const MAX_NAME: usize = 64;

/// The furthest into its line a YAML or TOML key, or a Markdown reference
/// label, may sit: a line indented or keyed further is not read as one.
const MAX_HEAD: usize = 1024;

/// The spans in `source` (the tracked file `file`) where the reference
/// `wanted` (already normalized) is used, at most `limit` of them. The
/// file's [`Page`] is read once, at its first candidate.
pub fn find_reference(file: &str, source: &str, wanted: &str, limit: usize) -> Vec<(usize, usize)> {
    let Some(kind) = kind_of(file) else {
        return Vec::new();
    };
    let page = OnceCell::new();
    crate::client_edit::spans_of(source, wanted, limit, |s, e| {
        used_at(page.get_or_init(|| Page::read(kind, source)), s, e)
    })
}

/// The kind of `file`, by its extension; `None` for a type that names no
/// image a site uses, or a file [`outside_the_site`].
fn kind_of(file: &str) -> Option<Kind> {
    if outside_the_site(file) {
        return None;
    }
    let ext = file.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase())?;
    kind_by_extension(&ext)
}

/// The kind of a file whose extension (lowercased) is `ext`.
fn kind_by_extension(ext: &str) -> Option<Kind> {
    Some(match ext {
        "html" | "htm" | "vue" | "svelte" | "astro" | "pug" | "jade" => Kind::Markup,
        "js" | "mjs" | "cjs" | "ts" | "mts" | "cts" | "jsx" | "tsx" => Kind::Code,
        "css" | "scss" | "sass" | "less" | "styl" | "pcss" => Kind::Style,
        "md" | "markdown" | "mdx" => Kind::Markdown,
        "yaml" | "yml" => Kind::Yaml,
        "toml" => Kind::Toml,
        "json" => Kind::Json,
        _ => return None,
    })
}

/// A repository's own documents, which are not the site's pages: a README,
/// changelog or history (`CHANGELOG-v2.md`, `CHANGES-2024.md` too), a
/// contributing guide, license, code of conduct or security policy, an
/// agent's instructions (`AGENTS.md`, `CLAUDE.md`), at any depth, and
/// anything in a `.github/` folder at any depth. A `docs/` folder is not
/// among them: it is often a docs site.
fn outside_the_site(file: &str) -> bool {
    let name = file.rsplit('/').next().unwrap_or(file).to_ascii_uppercase();
    let stem = name.split('.').next().unwrap_or("");
    let dated = ["CHANGELOG", "CHANGES", "HISTORY"].iter().any(|p| stem.starts_with(p));
    let named = matches!(
        stem,
        "README" | "CONTRIBUTING" | "LICENSE" | "LICENCE" | "CODE_OF_CONDUCT" | "SECURITY" | "AGENTS" | "CLAUDE"
    );
    dated || named || file.split('/').any(|part| part == ".github")
}

/// What the checks need from a file as a whole, read once in one pass per
/// kind of thing: where its lines end, where its quotes are, its scripts,
/// its front matter, and the ranges that are never a reference (code and
/// comments).
struct Page<'a> {
    src: &'a str,
    kind: Kind,
    /// Byte offsets of every `\n`.
    newlines: Vec<usize>,
    /// Byte offsets of every `"` and `'`.
    quotes: Vec<usize>,
    /// Byte offsets of every character a URL ends at (whitespace, a quote,
    /// `)`, `<`, `>`, `,`, an `&quot;`), for [`past_query`].
    stops: Vec<usize>,
    /// The bodies of `<script>` elements (and an Astro component's leading
    /// `---` fence), sorted.
    scripts: Vec<(usize, usize)>,
    /// A Markdown file's leading front matter: its body, and whether it is
    /// TOML (`+++`) rather than YAML (`---`).
    front: Option<(usize, usize, bool)>,
    /// Code and comments, merged and sorted: never a reference.
    skipped: Vec<(usize, usize)>,
}

impl<'a> Page<'a> {
    fn read(kind: Kind, src: &'a str) -> Page<'a> {
        let bytes = src.as_bytes();
        let at = |c: u8| bytes.iter().enumerate().filter(move |(_, b)| **b == c).map(|(i, _)| i);
        let newlines = at(b'\n').collect();
        let quotes = bytes.iter().enumerate().filter(|(_, b)| matches!(b, b'"' | b'\'')).map(|(i, _)| i).collect();
        let mut scripts = if kind == Kind::Markup { element_bodies(src, "script") } else { Vec::new() };
        if kind == Kind::Markup {
            scripts.extend(fenced(src, "---").map(|(a, b, _)| (a, b)));
            scripts.sort_unstable();
        }
        let front = if kind == Kind::Markdown { fenced(src, "---").or_else(|| fenced(src, "+++")) } else { None };
        let skipped = merged(skipped_ranges(kind, src, &scripts));
        let stops = src
            .char_indices()
            .filter(|(i, c)| c.is_whitespace() || matches!(c, '"' | '\'' | '`' | ')' | '<' | '>' | ',') || src[*i..].starts_with("&quot;"))
            .map(|(i, _)| i)
            .collect();
        Page { src, kind, newlines, quotes, stops, scripts, front, skipped }
    }

    /// Whether byte `at` is inside one of `ranges` (sorted, not overlapping).
    fn within(ranges: &[(usize, usize)], at: usize) -> bool {
        let i = ranges.partition_point(|r| r.0 <= at);
        i > 0 && at < ranges[i - 1].1
    }

    /// Where the line holding byte `at` starts.
    fn line_start(&self, at: usize) -> usize {
        let i = self.newlines.partition_point(|n| *n < at);
        if i == 0 { 0 } else { self.newlines[i - 1] + 1 }
    }

    /// The rest of the line from `at`, without its line break.
    fn rest_of_line(&self, at: usize) -> &'a str {
        let i = self.newlines.partition_point(|n| *n < at);
        let end = self.newlines.get(i).copied().unwrap_or(self.src.len());
        self.src[at..end].trim_end_matches('\r')
    }

    /// The last `"` or `'` before byte `at`.
    fn quote_before(&self, at: usize) -> Option<usize> {
        let i = self.quotes.partition_point(|q| *q < at);
        i.checked_sub(1).map(|i| self.quotes[i])
    }
}

/// `(start, end, toml)` of the body of the fence `fence` (`---` or `+++`)
/// that opens `src` on its own line, up to the line that closes it.
fn fenced(src: &str, fence: &str) -> Option<(usize, usize, bool)> {
    let rest = src.strip_prefix(fence)?;
    let body = fence.len() + rest.len() - rest.trim_start_matches(['\r']).len();
    if !src[body..].starts_with('\n') {
        return None;
    }
    let end = src[body..].find(&format!("\n{fence}"))? + body;
    Some((body, end + 1, fence == "+++"))
}

/// The bodies of every `<tag ...>` element in `src`: from after its `>` to
/// its `</tag` (or the end).
fn element_bodies(src: &str, tag: &str) -> Vec<(usize, usize)> {
    let (open, close) = (format!("<{tag}"), format!("</{tag}"));
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = src[from..].find(&open).map(|i| i + from) {
        let Some(body) = src[i..].find('>').map(|j| i + j + 1) else { break };
        let end = src[body..].find(&close).map_or(src.len(), |j| body + j);
        out.push((body, end));
        from = end;
    }
    out
}

/// The ranges a file of `kind` holds that are never a reference: comments,
/// and Markdown's code fences and spans.
fn skipped_ranges(kind: Kind, src: &str, scripts: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let whole = [(0, src.len())];
    match kind {
        Kind::Markup => {
            let mut out = delimited(src, "<!--", "-->");
            out.extend(scripts.iter().flat_map(|r| js_comments(src, *r)));
            out.extend(element_bodies(src, "style").into_iter().flat_map(|r| css_comments(src, r)));
            out
        }
        Kind::Code => js_comments(src, whole[0]),
        Kind::Style => css_comments(src, whole[0]),
        Kind::Markdown => {
            let mut out = delimited(src, "<!--", "-->");
            out.extend(markdown_code(src));
            out
        }
        Kind::Yaml | Kind::Toml | Kind::Json => Vec::new(),
    }
}

/// Every `open ... close` range in `src` (an unclosed one to the end).
fn delimited(src: &str, open: &str, close: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = src[from..].find(open).map(|i| i + from) {
        let end = src[i + open.len()..].find(close).map_or(src.len(), |j| i + open.len() + j + close.len());
        out.push((i, end));
        from = end;
    }
    out
}

/// The `/* ... */` comments in `src[start..end]`.
fn css_comments(src: &str, (start, end): (usize, usize)) -> Vec<(usize, usize)> {
    delimited(&src[..end], "/*", "*/").into_iter().filter(|r| r.0 >= start).collect()
}

/// What a JavaScript lexer is inside.
#[derive(Clone, Copy, PartialEq)]
enum Js {
    Code,
    Text(u8),
    Line(usize),
    Block(usize),
}

/// The `//` and `/* */` comments in the JavaScript at `src[start..end]`,
/// never inside a string (`"//cdn..."` is no comment). Regular-expression
/// literals are read as code.
fn js_comments(src: &str, (start, end): (usize, usize)) -> Vec<(usize, usize)> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let (mut state, mut i) = (Js::Code, start);
    while i < end {
        let (next, took) = js_step(state, b, i, &mut out);
        state = next;
        i += took;
    }
    if let Js::Line(from) | Js::Block(from) = state {
        out.push((from, end));
    }
    out
}

/// One step of [`js_comments`] at byte `i`: the state after it and how many
/// bytes it took. A comment that ends here is pushed to `out`.
fn js_step(state: Js, b: &[u8], i: usize, out: &mut Vec<(usize, usize)>) -> (Js, usize) {
    match state {
        Js::Code => js_code_step(b, i),
        _ => js_inner_step(state, b, i, out),
    }
}

/// A step in code: a string or a comment may open here.
fn js_code_step(b: &[u8], i: usize) -> (Js, usize) {
    match (b[i], b.get(i + 1)) {
        (c @ (b'"' | b'\'' | b'`'), _) => (Js::Text(c), 1),
        (b'/', Some(b'/')) => (Js::Line(i), 2),
        (b'/', Some(b'*')) => (Js::Block(i), 2),
        _ => (Js::Code, 1),
    }
}

/// A step inside a string or a comment: an escape is skipped; the string's
/// quote, a line comment's line break or a block comment's `*/` ends it.
fn js_inner_step(state: Js, b: &[u8], i: usize, out: &mut Vec<(usize, usize)>) -> (Js, usize) {
    let next = b.get(i + 1).copied();
    match (state, b[i]) {
        (Js::Text(_), b'\\') => (state, 2),
        (Js::Text(q), c) if c == q => (Js::Code, 1),
        (Js::Line(from), b'\n') => {
            out.push((from, i));
            (Js::Code, 1)
        }
        (Js::Block(from), b'*') if next == Some(b'/') => {
            out.push((from, i + 2));
            (Js::Code, 2)
        }
        _ => (state, 1),
    }
}

/// A Markdown file's code: fenced blocks (```` ``` ```` or `~~~` lines, to
/// the closing fence or the end) and, outside them, code spans (a run of
/// backticks to the next run of the same length).
fn markdown_code(src: &str) -> Vec<(usize, usize)> {
    let fences = code_fences(src);
    let mut runs = Vec::new();
    let b = src.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'`' || Page::within(&fences, i) {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && b[i] == b'`' {
            i += 1;
        }
        runs.push((start, i));
    }
    let mut out = fences;
    out.extend(paired_runs(&runs));
    out
}

/// Each run of backticks paired with the next run of the same length: the
/// spans they enclose, in one pass from the end.
fn paired_runs(runs: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut next_same = vec![None; runs.len()];
    let mut seen: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for (i, (a, b)) in runs.iter().enumerate().rev() {
        next_same[i] = seen.insert(b - a, i);
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < runs.len() {
        match next_same[i] {
            Some(j) => {
                out.push((runs[i].0, runs[j].1));
                i = j + 1;
            }
            None => i += 1,
        }
    }
    out
}

/// A Markdown file's fenced code blocks: from a line opening with three or
/// more backticks or tildes (up to three spaces in) to a line closing with
/// at least as many of the same, or the end.
fn code_fences(src: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut open: Option<(usize, u8, usize)> = None;
    let mut at = 0;
    for line in src.split_inclusive('\n') {
        match (open, fence_line(line)) {
            (None, Some((c, n, _))) => open = Some((at, c, n)),
            (Some((from, c, n)), Some((c2, n2, bare))) if c2 == c && n2 >= n && bare => {
                out.push((from, at + line.len()));
                open = None;
            }
            _ => {}
        }
        at += line.len();
    }
    if let Some((from, _, _)) = open {
        out.push((from, src.len()));
    }
    out
}

/// A Markdown fence line: its character, how many of it, and whether only
/// spaces follow (as a closing fence needs); `None` for any other line, or
/// one indented four spaces or more.
fn fence_line(line: &str) -> Option<(u8, usize, bool)> {
    let text = line.trim_start_matches(' ');
    if line.len() - text.len() > 3 {
        return None;
    }
    let c = *text.as_bytes().first().filter(|c| matches!(c, b'`' | b'~'))?;
    let run = text.bytes().take_while(|b| *b == c).count();
    (run >= 3).then(|| (c, run, text[run..].trim().is_empty()))
}

/// `ranges` sorted, with any that overlap merged.
fn merged(mut ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    ranges.sort_unstable();
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (a, b) in ranges {
        match out.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

/// Whether the match at `s..e` is a reference `page`'s file uses.
fn used_at(page: &Page, s: usize, e: usize) -> bool {
    if Page::within(&page.skipped, s) {
        return false;
    }
    let e = past_query(page, e);
    in_css_url(page.src, s, e) || used_as(page, s, e)
}

/// Whether the file uses a URL at `s..e` its own way (`url(...)` aside).
fn used_as(page: &Page, s: usize, e: usize) -> bool {
    let src = page.src;
    match page.kind {
        Kind::Markup => in_url_attribute(page, s, e) || (Page::within(&page.scripts, s) && in_code_literal(src, s, e)),
        Kind::Code => in_code_literal(src, s, e),
        Kind::Markdown => in_markdown(page, s, e),
        Kind::Yaml => in_yaml_value(page, s, e),
        Kind::Toml => in_toml_value(page, s, e),
        Kind::Json => in_quoted_value(src, s, e, &["\""]),
        Kind::Style => false,
    }
}

/// Where a Markdown file uses a URL: an image or link target, a reference
/// definition, a front matter value, an HTML attribute, an MDX import.
fn in_markdown(page: &Page, s: usize, e: usize) -> bool {
    let src = page.src;
    in_markdown_target(src, s, e)
        || in_reference_definition(page, s, e)
        || in_front_matter(page, s, e)
        || in_url_attribute(page, s, e)
        || (src[page.line_start(s)..].starts_with("import ") && in_code_literal(src, s, e))
}

/// `e` moved past a query or fragment that follows the reference (`?v=2`,
/// `#top`), to where the URL ends ([`Page::stops`]).
fn past_query(page: &Page, e: usize) -> usize {
    if !page.src[e..].starts_with(['?', '#']) {
        return e;
    }
    let i = page.stops.partition_point(|at| *at < e);
    page.stops.get(i).copied().unwrap_or(page.src.len())
}

/// The quotes a value may be wrapped in: in source, and as entities.
const QUOTES: &[&str] = &["\"", "'", "`", "&quot;", "&#39;", "&apos;"];

/// The quote (one of `quotes`) that ends right before `s`, and where it
/// starts, when the same one also follows at `e`.
fn quoted<'q>(src: &str, s: usize, e: usize, quotes: &[&'q str]) -> Option<(&'q str, usize)> {
    let q = quotes.iter().find(|q| src[..s].ends_with(**q) && src[e..].starts_with(**q))?;
    Some((q, s - q.len()))
}

/// Inside `url(...)`: the reference alone between the parentheses, quoted
/// (an entity too) or not, spaces allowed. Compared as bytes, so a
/// multi-byte character before it is never cut.
fn in_css_url(src: &str, s: usize, e: usize) -> bool {
    let (open, close) = match quoted(src, s, e, QUOTES) {
        Some((q, at)) => (at, e + q.len()),
        None => (s, e),
    };
    let left = src[..open].trim_end().as_bytes();
    let opened = left.len() >= 4 && left[left.len() - 4..].eq_ignore_ascii_case(b"url(");
    opened && src[close..].trim_start().starts_with(')')
}

/// The name `left` ends with: its last run of characters `part` accepts,
/// read back at most [`MAX_NAME`] characters (a longer run is no name).
fn name_at_end(left: &str, part: fn(char) -> bool) -> Option<&str> {
    let mut start = left.len();
    for (n, (i, c)) in left.char_indices().rev().enumerate() {
        if !part(c) {
            break;
        }
        if n >= MAX_NAME {
            return None;
        }
        start = i;
    }
    (start < left.len()).then(|| &left[start..])
}

fn attribute_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '-' | '_' | ':' | '@' | '.')
}

fn key_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '-' | '_')
}

/// The name of the attribute (or key) whose `=` ends at `at`, spaces
/// allowed around it: `src` in `src = "`.
fn attribute_before(src: &str, at: usize) -> Option<&str> {
    let left = trim_end_near(&src[..at])?.strip_suffix('=')?;
    name_at_end(trim_end_near(left)?, attribute_char)
}

/// `text` without its trailing whitespace, read back at most [`MAX_NAME`]
/// characters: `None` when more is whitespace (no attribute is spaced so).
fn trim_end_near(text: &str) -> Option<&str> {
    let mut end = text.len();
    for (n, (i, c)) in text.char_indices().rev().enumerate() {
        if !c.is_whitespace() {
            return Some(&text[..end]);
        }
        if n >= MAX_NAME {
            return None;
        }
        end = i;
    }
    Some(&text[..end])
}

/// `name` without a Vue binding's `:`, `v-bind:` or Svelte's `bind:`,
/// lowercased.
fn bare_attribute(name: &str) -> String {
    let name = name.strip_prefix("v-bind").unwrap_or(name);
    let name = name.strip_prefix("bind").filter(|n| n.starts_with(':')).unwrap_or(name);
    name.trim_start_matches(':').to_ascii_lowercase()
}

fn is_url_attribute(name: &str) -> bool {
    URL_ATTRIBUTES.contains(&bare_attribute(name).as_str())
}

/// The whole value of a URL attribute: `src="x"`, `src='x'`, a JSX or
/// Svelte expression (`src={"x"}`), a Vue binding (`:src="'x'"`), or one
/// candidate of a srcset.
fn in_url_attribute(page: &Page, s: usize, e: usize) -> bool {
    in_srcset(page, s, e)
        || match quoted(page.src, s, e, QUOTES) {
            Some((q, at)) => attribute_value_at(page.src, at, e + q.len()).is_some_and(is_url_attribute),
            None => false,
        }
}

/// The attribute whose value is the quoted literal at `open..close`: a
/// plain value (`name="`), inside `{...}` (`name={"`), or inside another
/// quote (`:name="'`).
fn attribute_value_at(src: &str, open: usize, close: usize) -> Option<&str> {
    if let Some(name) = attribute_before(src, open) {
        return Some(name);
    }
    let left = src[..open].trim_end();
    let wrapper = left.chars().last()?;
    let closer = match wrapper {
        '{' => '}',
        '"' | '\'' => wrapper,
        _ => return None,
    };
    src[close..].trim_start().starts_with(closer).then(|| attribute_before(src, left.len() - 1)).flatten()
}

/// One candidate of a srcset value: after its opening quote or a comma,
/// and followed by a space (its descriptor), a comma or the closing quote.
fn in_srcset(page: &Page, s: usize, e: usize) -> bool {
    let src = page.src;
    let Some(q) = page.quote_before(s) else {
        return false;
    };
    let quote = src[q..].chars().next().unwrap_or('"');
    let before = src[q + 1..s].trim_end();
    let first_or_next = before.is_empty() || before.ends_with(',');
    let ended = src[e..].starts_with(|c: char| c.is_whitespace() || c == ',' || c == quote);
    first_or_next
        && ended
        && attribute_before(src, q).map(bare_attribute).is_some_and(|n| SRCSET_ATTRIBUTES.contains(&n.as_str()))
}

/// A whole string literal (`"x"`, `'x'`, `` `x` ``), unless it is the value
/// of a text key or attribute (`alt="x"`, `alt: "x"`).
fn in_code_literal(src: &str, s: usize, e: usize) -> bool {
    in_quoted_value(src, s, e, &["\"", "'", "`"])
}

/// A whole value in one of `quotes`, unless it is a text key's.
fn in_quoted_value(src: &str, s: usize, e: usize, quotes: &[&str]) -> bool {
    match quoted(src, s, e, quotes) {
        Some((_, at)) => !text_key_before(src, at),
        None => false,
    }
}

/// Whether the value starting at `at` belongs to a text key or attribute:
/// `alt="`, `alt={"`, `alt: "`, `"alt": "`.
fn text_key_before(src: &str, at: usize) -> bool {
    let left = src[..at].trim_end();
    let left = left.strip_suffix('{').map_or(left, str::trim_end);
    match left.strip_suffix(':') {
        Some(k) => name_at_end(k.trim_end().trim_end_matches(['"', '\'']), key_char).is_some_and(is_text_key),
        None => attribute_before(src, left.len()).is_some_and(is_text_key),
    }
}

/// Whether `name` (at most [`MAX_NAME`] long) is one of [`TEXT_KEYS`].
fn is_text_key(name: &str) -> bool {
    name.len() <= MAX_NAME && TEXT_KEYS.contains(&bare_attribute(name).as_str())
}

/// Whether what follows `at` on its line is only spaces, or a ` #` comment.
fn ends_line(page: &Page, at: usize) -> bool {
    let rest = page.rest_of_line(at).trim_start();
    rest.is_empty() || rest.starts_with('#')
}

/// A Markdown image or link target: `](x)`, `](x "title")`, `](<x>)`.
fn in_markdown_target(src: &str, s: usize, e: usize) -> bool {
    let (open, close) = angled(src, s, e);
    if !src[..open].ends_with("](") {
        return false;
    }
    let rest = &src[close..];
    rest.starts_with(')') || (rest.starts_with([' ', '\t']) && rest.trim_start().starts_with(['"', '\'', '(']))
}

/// `s..e` widened by Markdown's optional `<...>` around a destination.
fn angled(src: &str, s: usize, e: usize) -> (usize, usize) {
    if src[..s].ends_with('<') && src[e..].starts_with('>') {
        (s - 1, e + 1)
    } else {
        (s, e)
    }
}

/// A Markdown reference definition: `[label]: x`, `[label]: <x> "title"`,
/// at the start of its line (up to three spaces in).
fn in_reference_definition(page: &Page, s: usize, e: usize) -> bool {
    let (open, close) = angled(page.src, s, e);
    let start = page.line_start(open);
    if open - start > MAX_HEAD {
        return false;
    }
    let head = page.src[start..open].trim_start_matches(' ');
    let defined = head.starts_with('[')
        && head.trim_end().strip_suffix(':').is_some_and(|h| h.ends_with(']'))
        && head.ends_with([' ', '\t']);
    defined && {
        let rest = page.rest_of_line(close);
        rest.is_empty() || rest.starts_with([' ', '\t'])
    }
}

/// A value in a Markdown file's leading front matter: YAML between `---`
/// lines, TOML between `+++` lines.
fn in_front_matter(page: &Page, s: usize, e: usize) -> bool {
    match page.front {
        Some((start, end, toml)) if (start..end).contains(&s) => {
            if toml {
                in_toml_value(page, s, e)
            } else {
                in_yaml_value(page, s, e)
            }
        }
        _ => false,
    }
}

/// A YAML value: `key: x` or a list item `- x`, quoted or not, alone on its
/// line (a ` #` comment after it allowed), unless its key is a text key or
/// the line is a comment.
fn in_yaml_value(page: &Page, s: usize, e: usize) -> bool {
    let (open, close) = match quoted(page.src, s, e, &["\"", "'"]) {
        Some((q, at)) => (at, e + q.len()),
        None => (s, e),
    };
    let start = page.line_start(open);
    if open - start > MAX_HEAD {
        return false;
    }
    let head = page.src[start..open].trim_start();
    let keyed = head.strip_suffix(' ').map(str::trim_end).and_then(|h| h.strip_suffix(':'));
    let valued = match keyed {
        Some(key) => !key.is_empty() && !key.starts_with('#') && !is_text_key(key.trim_matches(['"', '\''])),
        None => head == "- ",
    };
    valued && ends_line(page, close)
}

/// A TOML value: `key = "x"` or `key = 'x'`, alone on its line, unless its
/// key is a text key or the line is a comment.
fn in_toml_value(page: &Page, s: usize, e: usize) -> bool {
    let Some((q, at)) = quoted(page.src, s, e, &["\"", "'"]) else {
        return false;
    };
    let start = page.line_start(at);
    if at - start > MAX_HEAD {
        return false;
    }
    let head = page.src[start..at].trim();
    let key = head.strip_suffix('=').map(str::trim_end);
    key.is_some_and(|k| !k.is_empty() && !k.starts_with('#') && !is_text_key(k)) && ends_line(page, e + q.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans<'a>(file: &str, source: &'a str, wanted: &str) -> Vec<&'a str> {
        find_reference(file, source, wanted, 3).into_iter().map(|(a, b)| &source[a..b]).collect()
    }

    /// (review) Words in a page's text are never a reference it uses: not
    /// in a list, in quotes, in parentheses, in a Markdown code span, in alt
    /// text, or in a repository's own documents.
    #[test]
    fn prose_quotes_code_spans_alt_text_and_readmes_are_never_references() {
        for (file, source, wanted) in [
            ("page.html", "<p>Files: a.jpg, hero.jpg and b.jpg</p>", "hero.jpg"),
            ("page.html", "<p>Replace \"hero.jpg\" with yours.</p>", "hero.jpg"),
            ("page.html", "<p>Replace 'hero.jpg' or `hero.jpg`.</p>", "hero.jpg"),
            ("page.html", "<p>The hero (hero.jpg) is big.</p>", "hero.jpg"),
            ("post.md", "Use `/images/hero.jpg` for it.", "/images/hero.jpg"),
            ("post.md", "Files: a.jpg, hero.jpg\n\nThe hero (hero.jpg) \"hero.jpg\"", "hero.jpg"),
            ("data.yml", "alt: /hero.jpg\n", "/hero.jpg"),
            ("post.md", "# Post\n\nimage: /hero.jpg\n", "/hero.jpg"),
            ("post.md", "---\ntitle: x\n---\n\nimage: /hero.jpg\n", "/hero.jpg"),
            ("post.md", "# Post\n\nimage: /hero.jpg\n\n---\n\nMore.\n", "/hero.jpg"),
            ("post.md", "---\ntitle: x\n---\n\nimage: /hero.jpg\n\n---\n", "/hero.jpg"),
            ("README.md", "![logo](/hero.jpg)\n", "/hero.jpg"),
            ("docs/CHANGELOG.md", "![logo](/hero.jpg)\n", "/hero.jpg"),
            ("notes.txt", "src=\"/hero.jpg\"", "/hero.jpg"),
            ("HISTORY.md", "![logo](/hero.jpg)\n", "/hero.jpg"),
            ("CHANGELOG-v2.md", "![logo](/hero.jpg)\n", "/hero.jpg"),
            ("changes-2024.md", "![logo](/hero.jpg)\n", "/hero.jpg"),
            ("AGENTS.md", "![logo](/hero.jpg)\n", "/hero.jpg"),
            ("CLAUDE.md", "![logo](/hero.jpg)\n", "/hero.jpg"),
            ("site/.github/ISSUE_TEMPLATE/bug.md", "![logo](/hero.jpg)\n", "/hero.jpg"),
            ("page.html", "<p>画像は/hero.jpg</p>", "/hero.jpg"),
            ("page.html", "<p>🎉🎉/hero.jpg 🎉</p>", "/hero.jpg"),
            ("page.html", "<p>Café crème (/hero.jpg) é</p>", "/hero.jpg"),
            ("a.css", "/* 画像 */ .a { background: 画像/hero.jpg; }", "/hero.jpg"),
            ("data.yml", "# image: /hero.jpg\n", "/hero.jpg"),
            ("site.toml", "# image = \"/hero.jpg\"\n", "/hero.jpg"),
        ] {
            assert!(spans(file, source, wanted).is_empty(), "{file}: {source:?}");
        }
        for file in ["page.html", "App.jsx"] {
            let source = "<img alt=\"hero.jpg\" src=\"hero.jpg\" />";
            let found = find_reference(file, source, "hero.jpg", 3);
            assert_eq!(found.len(), 1, "{file}: alt text is not a reference");
            assert!(source[..found[0].0].ends_with("src=\""), "{file}: the src is");
        }
        let js = "const images = [{ src: \"/hero.jpg\", alt: \"/hero.jpg\" }];";
        assert_eq!(spans("main.ts", js, "/hero.jpg").len(), 1, "an alt key is text");
    }

    /// (review) Code and comments are not where a site uses an image:
    /// Markdown code fences and spans (an attribute inside one too), HTML
    /// comments, and JavaScript and CSS comments.
    #[test]
    fn code_blocks_and_comments_are_never_references() {
        for (file, source) in [
            ("post.md", "```html\n<img src=\"/hero.jpg\">\n![x](/hero.jpg)\n```\n"),
            ("post.md", "~~~\n[h]: /hero.jpg\n~~~\n"),
            ("post.md", "Use `<img src=\"/hero.jpg\">` here."),
            ("post.md", "Use `` ![x](/hero.jpg) `` here."),
            ("page.html", "<!-- <img src=\"/hero.jpg\"> -->"),
            ("post.md", "<!--\n![x](/hero.jpg)\n-->"),
            ("main.ts", "// const a = \"/hero.jpg\";\n"),
            ("main.ts", "/* import a from \"/hero.jpg\" */"),
            ("page.html", "<script>\n// load(\"/hero.jpg\")\n</script>"),
            ("a.css", "/* .a { background: url(/hero.jpg); } */"),
            ("page.html", "<style>/* url(/hero.jpg) */</style>"),
        ] {
            assert!(spans(file, source, "/hero.jpg").is_empty(), "{file}: {source:?}");
        }
        // What surrounds the code still counts, and `//` inside a string is no comment.
        let md = "```\n![x](/hero.jpg)\n```\n\n`code` and ![Hero](/hero.jpg)\n";
        assert_eq!(spans("post.md", md, "/hero.jpg").len(), 1);
        let ts = "const a = \"//cdn.x.com/a.jpg\"; const b = \"/hero.jpg\"; /* \"/hero.jpg\" */";
        assert_eq!(spans("main.ts", ts, "/hero.jpg").len(), 1);
        let html = "<!-- old --><img src=\"/hero.jpg\"><script>const u = 'a//b';</script><img src='/hero.jpg'>";
        assert_eq!(spans("page.html", html, "/hero.jpg").len(), 2);
    }

    /// (review) Matching is linear in the file: a 1 MiB page where nearly
    /// every character is a candidate is resolved quickly, and finds exactly
    /// the references it holds.
    #[test]
    fn a_large_page_full_of_candidates_is_resolved_quickly() {
        let size = 1024 * 1024;
        let prose = "<p>Here we see the entries every week, elsewhere eventually everyone enters.</p>\n";
        let body = prose.repeat(size / prose.len());
        let one_line = "var e=1;let ee=e+e;".repeat(size / 19);
        // Each holds what makes a scan long: a script or front matter at the
        // top, a megabyte with no quote, or one long line; and a few
        // references at the end, plus near-misses in a block, comment, alt.
        let pages = [
            (
                "page.html",
                format!("<script>var a = 1;</script>\n{body}<img src=\"e\" alt=\"e\"><!-- <img src=\"e\"> --><img src='e'>"),
                2,
            ),
            (
                "page.html",
                format!("<p>{}</p><script>let u = \"e\"; // \"e\"\n</script>", "e e, ".repeat(size / 5)),
                1,
            ),
            ("post.md", format!("---\ntitle: e\nimage: e\n---\n{body}```\n![x](e)\n```\n![e](e)\n"), 2),
            ("main.js", format!("{one_line}const a=\"e\";/*\"e\"*/const b='e';"), 2),
            ("data.yml", format!("a: {}\nimage: e\n", "e: ".repeat(size / 3)), 1),
            // A long name, or long whitespace, before a srcset's `=`; a long
            // indentation before a key; a long run of `?` after candidates.
            ("page.html", format!("<img {}=\"{}\">", "a".repeat(size / 2), "e, ".repeat(size / 6)), 0),
            ("page.html", format!("<img srcset{}=\"{}\">", " ".repeat(size / 2), "e, ".repeat(size / 6)), 0),
            ("data.yml", format!("{}a: {}\nimage: e\n", " ".repeat(size / 2), "e ".repeat(size / 4)), 1),
            ("post.md", format!("{}[a]: {}\n[b]: e\n", " ".repeat(size / 2), "e ".repeat(size / 4)), 1),
            ("page.html", format!("<p>{}</p><img src=\"e?v=2\">", "e?".repeat(size / 2)), 1),
        ];
        for (file, page, want) in &pages {
            let started = std::time::Instant::now();
            let found = find_reference(file, page, "e", usize::MAX);
            let took = started.elapsed();
            eprintln!("{file}: {} bytes, {} candidates checked: {took:?}", page.len(), page.matches('e').count());
            assert_eq!(found.len(), *want, "{file}: {found:?}");
            assert!(took < std::time::Duration::from_secs(1), "{file}: took {took:?}");
        }
    }

    /// (review) Each place a site names an image is found, exactly once.
    #[test]
    fn references_where_a_site_uses_them_are_found() {
        for (file, source) in [
            ("page.html", "<img src=\"/hero.jpg\">"),
            ("page.html", "<img src=\"/hero.jpg?v=2\">"),
            ("page.html", "<a href='/hero.jpg#top'>x</a>"),
            ("page.html", "<video poster=\"/hero.jpg\"></video>"),
            ("page.html", "<meta property=\"og:image\" content=\"/hero.jpg\">"),
            ("page.html", "<img data-src=\"/hero.jpg\">"),
            ("page.html", "<img srcset=\"/a.jpg 1x, /hero.jpg 2x\">"),
            ("page.html", "<img srcset=\"/hero.jpg 1x, /b.jpg 2x\">"),
            ("page.html", "<img srcset=\"/a.jpg 1x,\n\t/hero.jpg 2x\">"),
            ("page.html", "<div style=\"background:url(&quot;/hero.jpg&quot;)\"></div>"),
            ("page.html", "<div class=\"bg-[url(/hero.jpg)]\"></div>"),
            ("page.html", "<div class=\"bg-[url('/hero.jpg')]\"></div>"),
            ("page.html", "<script>const hero = \"/hero.jpg\";</script>"),
            ("page.html", "<img src=\"/hero.jpg\">\r\n<p>Hi</p>\r\n"),
            ("a.css", ".a { background: url(/hero.jpg); }"),
            ("a.css", ".a { background: url( \"/hero.jpg\" ); }"),
            ("App.jsx", "<img src={'/hero.jpg'} />"),
            ("App.tsx", "<img src={\"/hero.jpg\"} alt=\"Hero\" />"),
            ("App.vue", "<template><img :src=\"'/hero.jpg'\"></template>"),
            ("App.svelte", "<img src={\"/hero.jpg\"} alt=\"\">"),
            ("index.pug", "img(src=\"/hero.jpg\")"),
            ("main.ts", "import hero from \"/hero.jpg\";"),
            ("main.ts", "const hero = '/hero.jpg';"),
            ("main.ts", "const u = `/hero.jpg`;"),
            ("main.ts", "const Hero = styled.div`background: url(/hero.jpg);`;"),
            ("post.md", "![Hero](/hero.jpg \"title\")"),
            ("post.md", "![Hero](</hero.jpg>)"),
            ("post.md", "[h]: /hero.jpg\n"),
            ("post.md", "[h]: </hero.jpg> \"t\"\n"),
            ("post.md", "---\nimage: /hero.jpg\n---\n# Post\n"),
            ("post.md", "---\ntitle: x\nimage: \"/hero.jpg\" # cover\n---\n"),
            ("post.md", "+++\nimage = \"/hero.jpg\"\n+++\n"),
            ("post.md", "<img src=\"/hero.jpg\">\n"),
            ("data.yml", "hero:\n  - /hero.jpg\n  src: /hero.jpg"),
            ("site.toml", "image = '/hero.jpg'\n"),
            ("data.json", "{\"image\": \"/hero.jpg\"}"),
        ] {
            let found = spans(file, source, "/hero.jpg");
            let want = if source.contains("  - /hero.jpg") { 2 } else { 1 };
            assert_eq!(found.len(), want, "{file}: {source:?} -> {found:?}");
            assert!(found.iter().all(|s| *s == "/hero.jpg"), "{file}: {found:?}");
        }
        assert_eq!(spans("page.html", "<img src=\"/héros/été.jpg\">", "/héros/été.jpg"), ["/héros/été.jpg"]);
        assert_eq!(spans("page.html", "<img src=\"/画像/ヒーロー.jpg\">", "/画像/ヒーロー.jpg"), ["/画像/ヒーロー.jpg"]);
        for source in [
            "<p>画像は</p><img src=\"/hero.jpg\">",
            "<img alt=\"🎉🎉\" src=\"/hero.jpg\"> 🎉",
            "<p>Café crème</p><div style=\"background:url(/hero.jpg)\">é</div>",
            "<div style=\"background:url(画像/hero.jpg)\"></div><img src=\"/hero.jpg\">",
        ] {
            assert_eq!(spans("page.html", source, "/hero.jpg"), ["/hero.jpg"], "{source}");
        }
        assert_eq!(spans("post.md", "---\ntitle: 画像\nimage: /hero.jpg\n---\n", "/hero.jpg"), ["/hero.jpg"]);
        assert_eq!(spans("page.html", "<img src=\"/a.jpg?x=1&amp;y=2\">", "/a.jpg?x=1&y=2"), ["/a.jpg?x=1&amp;y=2"]);
    }

    /// (review) A reference is a whole URL: never the tail or the head of a
    /// longer one, whatever URL characters surround it.
    #[test]
    fn a_reference_is_never_part_of_a_longer_url() {
        for (source, wanted) in [
            ("<img src=\"/assets/hero.jpg\">", "hero.jpg"),
            ("<img src=\"/hero.jpg.bak\">", "/hero.jpg"),
            ("<img src=\"my_hero.jpg\"><img src=\"x-hero.jpg\">", "hero.jpg"),
            ("<img src=\"/img/logo@2x.png\">", "2x.png"),
            ("<img src=\"https://cdn.x.com/hero.jpg\">", "//cdn.x.com/hero.jpg"),
            ("<img src=\"../hero.jpg\">", "./hero.jpg"),
            ("<div style=\"background:url(/a/hero.jpg)\"></div>", "hero.jpg"),
            ("<img srcset=\"/a/hero.jpg 1x, /b/hero.jpg 2x\">", "hero.jpg"),
        ] {
            assert!(spans("page.html", source, wanted).is_empty(), "{source}");
        }
        for joined in ["/a+hero.jpg", "/a~hero.jpg", "/a:hero.jpg", "/a,hero.jpg", "/a=hero.jpg", "/a%20hero.jpg", "/a!hero.jpg"] {
            assert!(spans("page.html", &format!("<img src=\"{joined}\">"), "hero.jpg").is_empty(), "{joined}");
        }
        assert!(spans("main.ts", "const u = `${base}/hero.jpg`;", "/hero.jpg").is_empty());
        assert!(spans("post.md", "![Hero](/a/hero.jpg)", "hero.jpg").is_empty());
    }
}
