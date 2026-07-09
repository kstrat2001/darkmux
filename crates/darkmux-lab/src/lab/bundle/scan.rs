//! Function scanning: locate functions in TS/TSX source text, extract their
//! params/calls/branches, and the mechanical noise/ambient tables the fact
//! families filter against.
//!
//! Ported from `bundler.py` (Phase A reference). **No `regex` crate** — the
//! workspace's dependency discipline (see repo `CLAUDE.md`, "don't add
//! dependencies casually") rules it out here, so every pattern below is a
//! hand-rolled byte/char scanner. Two properties keep this safe over
//! arbitrary UTF-8 source: (1) every delimiter this module tests for
//! (`(`, `)`, `{`, `}`, `<`, `>`, `:`, `=`, `,`, `.`) is single-byte ASCII,
//! so any byte offset landing on one of them is always a valid `str` char
//! boundary; (2) the few places that fall back to "last byte scanned" when
//! a scan runs off its bound (no regex backtracking equivalent) explicitly
//! round down to a char boundary before slicing (`floor_boundary`).

use std::collections::HashSet;
use std::sync::OnceLock;

// ---------------------------------------------------------------------
// Byte/char classification
// ---------------------------------------------------------------------

fn is_ident_start_byte(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b'$'
}

fn is_ident_cont_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// `\w` scope used by the reference's `\b` boundary checks — ASCII
/// alnum + underscore. Two documented rare edges (accepted for golden
/// stability — fidelity on the common path beats chasing them):
///
/// - Python's default `\w` is unicode-aware; this is ASCII-only, so a
///   NON-ASCII identifier character adjacent to a match reads as a word
///   boundary here where Python's would not. TS identifiers in the
///   validated corpus are ASCII throughout.
/// - `$` is EXCLUDED, exactly matching Python's `\w` — so `count_refs`
///   on param `x` counts the `x` inside `$x` as a boundary-satisfying
///   hit in BOTH implementations. The `$`-prefixed-identifier boundary
///   is a shared quirk inherited from the reference, not a port delta.
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Round `idx` down to the nearest `str` char boundary at or before it.
/// Used only where a scan can run off its bound without ever landing on a
/// known-ASCII delimiter (the reference's Python indexing is always safe
/// since Python strings index by codepoint; Rust's byte-indexed slicing
/// needs this guard for the same fallback paths).
fn floor_boundary(s: &str, mut idx: usize) -> usize {
    let n = s.len();
    if idx > n {
        idx = n;
    }
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

// ---------------------------------------------------------------------
// NOISE / ORM_NOISE / KEYWORD_NAMES / STEM_STOPLIST / TS_EXCLUDE_DIRS
// ---------------------------------------------------------------------

/// Generic-language noise call names — never worth a call-fact line.
const NOISE_WORDS: &[&str] = &[
    "if", "for", "while", "switch", "catch", "return", "function", "await", "async", "typeof",
    "new", "delete", "void", "const", "let", "var", "import", "export", "from", "class",
    "extends", "implements", "interface", "type", "throw", "super", "this", "yield", "in", "of",
    "do", "else", "case", "break", "continue", "try", "finally", "default", "enum", "namespace",
    "as", "is", "keyof", "map", "filter", "forEach", "reduce", "find", "some", "every",
    "flatMap", "concat", "slice", "splice", "push", "pop", "shift", "join", "split", "trim",
    "replace", "match", "test", "includes", "startsWith", "endsWith", "toLowerCase",
    "toUpperCase", "toString", "Number", "String", "Array", "Object", "Boolean", "Symbol",
    "BigInt", "console", "Math", "JSON", "Promise", "Set", "Map", "WeakMap", "parseInt",
    "parseFloat", "isNaN", "isFinite", "Date", "RegExp", "Error", "TypeError", "RangeError",
    "require", "module", "then", "finally", "all", "race", "resolve", "reject", "keys",
    "values", "entries", "assign", "freeze", "create", "length", "name", "value", "data",
    "result", "error", "status", "code", "message", "id", "key", "index", "count", "total",
    "sum",
];

/// Lucid/AdonisJS ORM query-builder chain verbs — near-universal noise in
/// the corpus this bundler was validated against; excluded so fact budget
/// goes to calls that carry real signal (project functions, ambient-time
/// reads). Ported verbatim from `ORM_NOISE`.
const ORM_NOISE_WORDS: &[&str] = &[
    "query", "where", "whereRaw", "whereIn", "whereNotIn", "whereNull", "whereNotNull",
    "orderBy", "select", "first", "firstOrFail", "save", "related", "preload", "load", "from",
    "insertQuery", "update", "delete", "all", "paginate", "exec", "merge", "fill",
    "useTransaction", "pluck", "count", "sum", "avg", "min", "max", "groupBy", "having",
    "limit", "offset", "apply", "orWhere", "andWhere", "whereBetween", "whereLike",
];

fn noise_set() -> &'static HashSet<&'static str> {
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| {
        NOISE_WORDS
            .iter()
            .chain(ORM_NOISE_WORDS.iter())
            .copied()
            .collect()
    })
}

pub fn is_noise(name: &str) -> bool {
    noise_set().contains(name)
}

/// Names `NAME_METHOD_RE` candidates are filtered against (control-flow
/// keywords etc. that also happen to look like `ident(` at a line start).
/// NOTE: this filter applies ONLY to method-style candidates in the
/// reference — arrow/`function`-keyword candidates are never checked
/// against it (ported faithfully, not "fixed").
const KEYWORD_NAMES: &[&str] = &[
    "if", "for", "while", "switch", "catch", "else", "constructor", "function", "return",
    "typeof", "new", "await", "yield", "case",
];

fn is_keyword_name(name: &str) -> bool {
    KEYWORD_NAMES.contains(&name)
}

const STEM_STOPLIST: &[&str] = &["get", "set", "is", "has", "the", "and", "all", "new", "for", "not"];

/// Directories `iter_ts_files` never descends into.
pub const TS_EXCLUDE_DIRS: &[&str] = &["node_modules", "dist", "build", ".git", "tmp", "migrations"];

/// `path.endswith((".ts", ".tsx")) and not path.startswith("tests/") and
/// "test" not in os.path.basename(path).lower()`.
pub fn ts_file(path: &str) -> bool {
    if !(path.ends_with(".ts") || path.ends_with(".tsx")) {
        return false;
    }
    if path.starts_with("tests/") {
        return false;
    }
    let base = path.rsplit('/').next().unwrap_or(path);
    !base.to_lowercase().contains("test")
}

// ---------------------------------------------------------------------
// AMBIENT_PATTERNS
// ---------------------------------------------------------------------

/// Literal-substring search for `prefix` followed by `\s*(` anywhere in
/// `text` — hand-rolled equivalent of e.g. `re.search(r"Math\.random\s*\(",
/// text)`. Deliberately has NO word-boundary check before `prefix`
/// (neither does the reference's regex), so e.g. `myMath.random(` also
/// matches — ported faithfully.
fn call_after(text: &str, prefix: &str) -> bool {
    for (idx, _) in text.match_indices(prefix) {
        let rest = &text[idx + prefix.len()..];
        let trimmed = rest.trim_start_matches(char::is_whitespace);
        if trimmed.starts_with('(') {
            return true;
        }
    }
    false
}

/// `new\s+Date\s*\(` — same no-boundary-before-`new` fidelity note as
/// `call_after`.
fn new_date_call(text: &str) -> bool {
    for (idx, _) in text.match_indices("new") {
        let rest = &text[idx + 3..];
        let ws_len = rest.len() - rest.trim_start_matches(char::is_whitespace).len();
        if ws_len == 0 {
            continue; // `\s+` requires at least one whitespace char
        }
        let after_ws = &rest[ws_len..];
        if let Some(after_date) = after_ws.strip_prefix("Date") {
            let paren = after_date.trim_start_matches(char::is_whitespace);
            if paren.starts_with('(') {
                return true;
            }
        }
    }
    false
}

/// First matching ambient-read pattern in `text`, checked in the
/// reference's priority order, or `None`.
pub fn ambient_label(text: &str) -> Option<&'static str> {
    if call_after(text, "DateTime.local") {
        return Some("DateTime.local()");
    }
    if call_after(text, "DateTime.now") {
        return Some("DateTime.now()");
    }
    if call_after(text, "Date.now") {
        return Some("Date.now()");
    }
    if new_date_call(text) {
        return Some("new Date(");
    }
    if text.contains("process.env") {
        return Some("process.env");
    }
    if call_after(text, "Math.random") {
        return Some("Math.random()");
    }
    None
}

// ---------------------------------------------------------------------
// Function location + parsing
// ---------------------------------------------------------------------

/// One located function: `start0`/`end0` are 0-indexed line numbers
/// (inclusive) into the file's `lines`.
#[derive(Debug, Clone)]
pub struct FnDef {
    pub start0: usize,
    pub end0: usize,
    pub header: String,
    pub name: String,
}

/// Port of `find_body_brace`: from the char offset of a param list's
/// closing `)`, find the body's opening `{`, depth-aware over `<>`/`()`
/// so a return-type generic containing its own braces (e.g.
/// `Promise<{ totalRevenue: number }>`) isn't mistaken for the body. Also
/// rejects the two false-positive classes the reference's docstring
/// documents: a call-site argument line (next non-ws char after the
/// `)` is `,`/`)`/`;`/`=` at depth 0 — never a valid declaration tail),
/// and a cast/index expression (`(x as Foo)[key]` / `(x as Foo).bar`).
pub fn find_body_brace(text: &str, pclose: usize, max_scan: usize) -> Option<usize> {
    let b = text.as_bytes();
    let n = b.len();
    let mut i = pclose + 1;
    let limit = (pclose + 1 + max_scan).min(n);
    let mut depth: i32 = 0;
    let mut first = true;
    while i < limit {
        let c = b[i];
        if c == b' ' || c == b'\t' || c == b'\r' || c == b'\n' {
            i += 1;
            continue;
        }
        if first {
            first = false;
            let is_arrow = c == b'=' && i + 1 < n && b[i + 1] == b'>';
            if c != b'{' && c != b':' && !is_arrow {
                return None;
            }
        }
        if c == b'<' || c == b'(' {
            depth += 1;
            i += 1;
            continue;
        }
        if c == b'>' || c == b')' {
            depth -= 1;
            i += 1;
            continue;
        }
        if c == b'{' {
            if depth <= 0 {
                return Some(i);
            }
            i += 1;
            continue;
        }
        if c == b'=' && i + 1 < n && b[i + 1] == b'>' {
            i += 2;
            continue;
        }
        if (c == b',' || c == b';' || c == b'=') && depth <= 0 {
            return None;
        }
        i += 1;
    }
    None
}

fn skip_ws(b: &[u8], mut pos: usize) -> usize {
    while pos < b.len() && matches!(b[pos], b' ' | b'\t') {
        pos += 1;
    }
    pos
}

fn skip_ws1(b: &[u8], pos: usize) -> Option<usize> {
    let np = skip_ws(b, pos);
    if np > pos {
        Some(np)
    } else {
        None
    }
}

fn eat_literal(s: &str, pos: usize, lit: &str) -> Option<usize> {
    if s.get(pos..)?.starts_with(lit) {
        Some(pos + lit.len())
    } else {
        None
    }
}

fn eat_ident(s: &str, pos: usize) -> Option<(usize, &str)> {
    let b = s.as_bytes();
    if pos >= b.len() || !is_ident_start_byte(b[pos]) {
        return None;
    }
    let mut i = pos + 1;
    while i < b.len() && is_ident_cont_byte(b[i]) {
        i += 1;
    }
    Some((i, &s[pos..i]))
}

/// Line-anchored equivalent of `NAME_ARROW_RE` — matches at column 0
/// of `line` only (a Python `re.M` `^`-anchored match's span always
/// starts exactly at the line's first character; see module docs).
fn match_name_arrow(line: &str) -> Option<String> {
    let b = line.as_bytes();
    let mut pos = skip_ws(b, 0);
    if let Some(p) = eat_literal(line, pos, "export") {
        if let Some(p2) = skip_ws1(b, p) {
            pos = p2;
        }
    }
    if let Some(p) = eat_literal(line, pos, "default") {
        if let Some(p2) = skip_ws1(b, p) {
            pos = p2;
        }
    }
    let p = eat_literal(line, pos, "const").or_else(|| eat_literal(line, pos, "let"))?;
    let p = skip_ws1(b, p)?;
    let (p, name) = eat_ident(line, p)?;
    let name = name.to_string();
    let mut p = skip_ws(b, p);
    // optional `(?::[^=]+)?` — a type annotation with >=1 non-'=' char.
    if p < b.len() && b[p] == b':' {
        let start = p + 1;
        let mut q = start;
        while q < b.len() && b[q] != b'=' {
            q += 1;
        }
        if q == start {
            return None; // `:` present but empty annotation body -> no match
        }
        p = q;
    }
    p = skip_ws(b, p);
    if p >= b.len() || b[p] != b'=' {
        return None;
    }
    p += 1;
    p = skip_ws(b, p);
    if let Some(pa) = eat_literal(line, p, "async") {
        p = skip_ws(b, pa);
    }
    if p < b.len() && b[p] == b'(' {
        Some(name)
    } else {
        None
    }
}

const METHOD_MODIFIERS: [&str; 7] = ["public", "private", "protected", "static", "async", "get", "set"];

/// Line-anchored equivalent of `NAME_METHOD_RE`.
fn match_name_method(line: &str) -> Option<String> {
    let b = line.as_bytes();
    let mut pos = skip_ws(b, 0);
    loop {
        let mut advanced = false;
        for kw in METHOD_MODIFIERS {
            if let Some(p) = eat_literal(line, pos, kw) {
                if let Some(p2) = skip_ws1(b, p) {
                    pos = p2;
                    advanced = true;
                    break;
                }
            }
        }
        if !advanced {
            break;
        }
    }
    let (p, name) = eat_ident(line, pos)?;
    let name = name.to_string();
    let mut p = skip_ws(b, p);
    if p < b.len() && b[p] == b'<' {
        let rel = line[p + 1..].find('>')?;
        p = p + 1 + rel + 1;
        p = skip_ws(b, p);
    }
    if p < b.len() && b[p] == b'(' {
        Some(name)
    } else {
        None
    }
}

/// Equivalent of `NAME_FN_RE.finditer(text)` — scans the WHOLE joined
/// text (not line-anchored) for `\bfunction\s+(ident)` occurrences.
fn scan_function_keyword(text: &str) -> Vec<(usize, String)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    for (idx, _) in text.match_indices("function") {
        if idx > 0 && is_word_byte(bytes[idx - 1]) {
            continue; // `\b` boundary before "function"
        }
        let after = idx + "function".len();
        let mut j = after;
        while j < bytes.len() && matches!(bytes[j], b' ' | b'\t' | b'\r' | b'\n') {
            j += 1;
        }
        if j == after {
            continue; // `\s+` requires >=1 whitespace char
        }
        let Some((k, name)) = eat_ident(text, j) else {
            continue;
        };
        let mut m = k;
        while m < bytes.len() && matches!(bytes[m], b' ' | b'\t' | b'\r' | b'\n') {
            m += 1;
        }
        if m < bytes.len() && bytes[m] == b'(' {
            out.push((idx, name.to_string()));
        }
    }
    out
}

/// Port of `find_all_functions_in_text`: offset-based scan over the WHOLE
/// FILE text so multi-line signatures (params spanning several lines
/// before the body's opening `{`) are handled correctly.
pub fn find_all_functions_in_text(lines: &[String]) -> Vec<FnDef> {
    if lines.is_empty() {
        return Vec::new();
    }
    let text = lines.join("\n");
    let mut offsets = Vec::with_capacity(lines.len() + 1);
    offsets.push(0usize);
    for l in lines {
        let last = *offsets.last().unwrap();
        offsets.push(last + l.len() + 1);
    }
    let line_of = |off: usize| -> usize {
        let mut lo = 0usize;
        let mut hi = lines.len() - 1;
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            if offsets[mid] <= off {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        lo
    };

    let mut candidates: Vec<(usize, String)> = Vec::new();
    candidates.extend(scan_function_keyword(&text));
    for (line_idx, line) in lines.iter().enumerate() {
        if let Some(name) = match_name_arrow(line) {
            candidates.push((offsets[line_idx], name));
        }
    }
    for (line_idx, line) in lines.iter().enumerate() {
        if let Some(name) = match_name_method(line) {
            if is_keyword_name(&name) {
                continue;
            }
            candidates.push((offsets[line_idx], name));
        }
    }

    let b = text.as_bytes();
    let mut results: Vec<FnDef> = Vec::new();
    let mut seen: HashSet<(usize, usize)> = HashSet::new();
    for (start_off, name) in candidates {
        if start_off > text.len() {
            continue;
        }
        let Some(popen_rel) = text[start_off..].find('(') else {
            continue;
        };
        let popen = start_off + popen_rel;
        if popen - start_off > 60 {
            continue;
        }
        let scan_end = (popen + 4000).min(b.len());
        let mut depth = 0i32;
        let mut pclose: Option<usize> = None;
        let mut k = popen;
        while k < scan_end {
            match b[k] {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        pclose = Some(k);
                        break;
                    }
                }
                _ => {}
            }
            k += 1;
        }
        let Some(pclose) = pclose else { continue };
        let Some(brace_pos) = find_body_brace(&text, pclose, 300) else {
            continue;
        };
        let mut depth2 = 0i32;
        let mut close_pos: Option<usize> = None;
        let mut m = brace_pos;
        while m < b.len() {
            match b[m] {
                b'{' => depth2 += 1,
                b'}' => {
                    depth2 -= 1;
                    if depth2 == 0 {
                        close_pos = Some(m);
                        break;
                    }
                }
                _ => {}
            }
            m += 1;
        }
        let Some(close_pos) = close_pos else { continue };
        let start_line = line_of(start_off);
        let end_line = line_of(close_pos);
        let key = (start_line, end_line);
        if seen.contains(&key) || end_line.saturating_sub(start_line) > 400 {
            continue;
        }
        seen.insert(key);
        results.push(FnDef {
            start0: start_line,
            end0: end_line,
            header: lines[start_line].trim().to_string(),
            name,
        });
    }
    results.sort_by_key(|f| f.start0);
    results
}

/// 1-indexed target line -> tightest enclosing `FnDef`, or `None`.
pub fn enclosing_fn_for_line(all_fns: &[FnDef], ln1: u32) -> Option<&FnDef> {
    let ln0 = ln1.saturating_sub(1) as usize;
    let mut best: Option<&FnDef> = None;
    for f in all_fns {
        if f.start0 <= ln0 && ln0 <= f.end0 {
            match best {
                None => best = Some(f),
                Some(b) if (f.end0 - f.start0) < (b.end0 - b.start0) => best = Some(f),
                _ => {}
            }
        }
    }
    best
}

// ---------------------------------------------------------------------
// Param list splitting (shared by extract_params / extract_calls args)
// ---------------------------------------------------------------------

/// Split `s` on top-level commas — depth-aware over `([{<` / `)]}>` so a
/// nested type/generic/object doesn't get split. Empty (whitespace-only)
/// parts are dropped, matching the reference's two call sites (the
/// `split_top_commas` helper AND `extract_params`'s inlined copy of the
/// identical algorithm collapse into this one function here).
pub fn split_top_commas(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut d: i32 = 0;
    for ch in s.chars() {
        match ch {
            '(' | '[' | '{' | '<' => d += 1,
            ')' | ']' | '}' | '>' => d -= 1,
            _ => {}
        }
        if ch == ',' && d == 0 {
            parts.push(std::mem::take(&mut cur));
        } else {
            cur.push(ch);
        }
    }
    if !cur.trim().is_empty() {
        parts.push(cur);
    }
    parts.into_iter().filter(|p| !p.trim().is_empty()).collect()
}

/// From the function's start line, capture the full (possibly multi-line)
/// param list's raw `(...)` interior. `None` if no `(` is found in the
/// 15-line scan window (matches the reference's `extract_params` window).
fn param_list_inner(lines: &[String], start0: usize) -> Option<String> {
    if start0 >= lines.len() {
        return None;
    }
    let window_end = (start0 + 15).min(lines.len());
    let text = lines[start0..window_end].join("\n");
    let i = text.find('(')?;
    let b = text.as_bytes();
    let mut depth = 0i32;
    let mut j = i;
    for (k, &byte) in b.iter().enumerate().skip(i) {
        j = k;
        match byte {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
    }
    let end = floor_boundary(&text, j);
    let start = (i + 1).min(text.len());
    if start > end {
        return Some(String::new());
    }
    Some(text[start..end].to_string())
}

fn param_name(raw: &str) -> Option<String> {
    let p = raw.trim();
    if p.is_empty() || p.starts_with('{') || p.starts_with('[') {
        return None;
    }
    let p = p.trim_start_matches('.');
    let b = p.as_bytes();
    if b.is_empty() || !is_ident_start_byte(b[0]) {
        return None;
    }
    let mut i = 1;
    while i < b.len() && is_ident_cont_byte(b[i]) {
        i += 1;
    }
    Some(p[..i].to_string())
}

/// Port of `extract_params`: param names only (destructured/rest-ambiguous
/// params skipped, matching the reference).
pub fn extract_params(lines: &[String], start0: usize) -> Vec<String> {
    let Some(inner) = param_list_inner(lines, start0) else {
        return Vec::new();
    };
    split_top_commas(&inner)
        .iter()
        .filter_map(|p| param_name(p))
        .collect()
}

/// `rest` is the param text right after the identifier name (e.g.
/// `": number = 5"`, `" = 5"`, `""`). Depth-aware scan for a top-level
/// `=` that isn't part of `==`/`=>`/`<=`/`>=`/`!=`, returning the
/// trimmed text after it. **New logic (#1222 packet 3) — no Python
/// precedent**; the reference never extracted default values, only
/// param names (it deliberately throws away everything after the name).
fn find_default_rhs(rest: &str) -> Option<String> {
    let chars: Vec<char> = rest.chars().collect();
    let mut d: i32 = 0;
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '(' | '[' | '{' | '<' => d += 1,
            ')' | ']' | '}' | '>' => d -= 1,
            _ => {}
        }
        if c == '=' && d == 0 {
            let prev = if i > 0 { Some(chars[i - 1]) } else { None };
            let next = chars.get(i + 1).copied();
            if next == Some('=') || next == Some('>') || matches!(prev, Some('=') | Some('!') | Some('<') | Some('>')) {
                i += 1;
                continue;
            }
            let default_text: String = chars[i + 1..].iter().collect();
            let trimmed = default_text.trim();
            return if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            };
        }
        i += 1;
    }
    None
}

/// Default-parameter facts input (#1222 packet 3 mandate — new, no Python
/// precedent): `(name, default_expr)` for every non-destructured,
/// non-rest param that declares a default value.
pub fn extract_param_defaults(lines: &[String], start0: usize) -> Vec<(String, String)> {
    let Some(inner) = param_list_inner(lines, start0) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for raw in split_top_commas(&inner) {
        let p = raw.trim();
        if p.is_empty() || p.starts_with('{') || p.starts_with('[') {
            continue;
        }
        let p = p.trim_start_matches('.');
        let b = p.as_bytes();
        if b.is_empty() || !is_ident_start_byte(b[0]) {
            continue;
        }
        let mut k = 1;
        while k < b.len() && is_ident_cont_byte(b[k]) {
            k += 1;
        }
        let name = &p[..k];
        if let Some(default_expr) = find_default_rhs(&p[k..]) {
            out.push((name.to_string(), default_expr));
        }
    }
    out
}

/// `\b{name}\b` occurrence count — substring search with manual word-
/// boundary checks either side.
pub fn count_refs(text: &str, name: &str) -> usize {
    if name.is_empty() {
        return 0;
    }
    let b = text.as_bytes();
    let mut count = 0;
    for (idx, _) in text.match_indices(name) {
        let before_ok = idx == 0 || !is_word_byte(b[idx - 1]);
        let after_idx = idx + name.len();
        let after_ok = after_idx >= b.len() || !is_word_byte(b[after_idx]);
        if before_ok && after_ok {
            count += 1;
        }
    }
    count
}

// ---------------------------------------------------------------------
// Call-site extraction
// ---------------------------------------------------------------------

/// Maximal `[A-Za-z_$][\w$]*` identifier runs in `text`, as `(start,end)`
/// byte offsets — the shared tokenization `extract_calls` walks to find
/// `(prefix.)?name(` call sites (see module docs for why byte-level
/// scanning here is safe over arbitrary UTF-8).
fn ident_tokens(text: &str) -> Vec<(usize, usize)> {
    let b = text.as_bytes();
    let n = b.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        if is_ident_start_byte(b[i]) {
            let start = i;
            i += 1;
            while i < n && is_ident_cont_byte(b[i]) {
                i += 1;
            }
            out.push((start, i));
        } else {
            i += 1;
        }
    }
    out
}

/// Scan `text[open_idx..]` (bounded to 400 bytes) for the `)` that closes
/// the `(` at `open_idx`, depth-aware. Falls back to the last byte
/// scanned when no match closes within the window — matches the
/// reference's Python for-loop variable-leakage semantics (a truncated
/// `args` slice on a runaway/unclosed call), rounded to a char boundary
/// for safe slicing.
fn find_call_close(text: &str, open_idx: usize) -> usize {
    let b = text.as_bytes();
    let n = b.len();
    let limit = (open_idx + 400).min(n);
    if limit <= open_idx {
        return open_idx;
    }
    let mut depth: i32 = 0;
    let mut last = open_idx;
    for (k, &byte) in b.iter().enumerate().take(limit).skip(open_idx) {
        last = k;
        match byte {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return k;
                }
            }
            _ => {}
        }
    }
    floor_boundary(text, last)
}

/// Port of `extract_calls`: `(bare_name, display_name, argc)` for every
/// call site in `text`, filtering `NOISE`/`ORM_NOISE` and capitalized
/// (constructor/class-like) names. `display_name` keeps a single-level
/// object prefix (`DateTime.local`) when present; `bare_name` (the last
/// identifier) drives noise filtering + callee resolution.
pub fn extract_calls(text: &str) -> Vec<(String, String, usize)> {
    let tokens = ident_tokens(text);
    let b = text.as_bytes();
    let mut out = Vec::new();
    for i in 0..tokens.len() {
        let (name_start, name_end) = tokens[i];
        let name = &text[name_start..name_end];
        let mut p = name_end;
        while p < b.len() && matches!(b[p], b' ' | b'\t' | b'\r' | b'\n') {
            p += 1;
        }
        if p >= b.len() || b[p] != b'(' {
            continue;
        }
        if is_noise(name) || name.chars().next().map(|c| c.is_ascii_uppercase()).unwrap_or(false) {
            continue;
        }
        let mut prefix: Option<&str> = None;
        if name_start > 0 && b[name_start - 1] == b'.' {
            let dot_pos = name_start - 1;
            if i > 0 {
                let (pstart, pend) = tokens[i - 1];
                if pend == dot_pos {
                    prefix = Some(&text[pstart..pend]);
                }
            }
        }
        let close = find_call_close(text, p);
        let arg_start = (p + 1).min(text.len());
        let arg_end = close.max(arg_start).min(text.len());
        let args = &text[arg_start..arg_end];
        let argc = if args.trim().is_empty() {
            0
        } else {
            split_top_commas(args).len()
        };
        let display = match prefix {
            Some(pre) => format!("{pre}.{name}"),
            None => name.to_string(),
        };
        out.push((name.to_string(), display, argc));
    }
    out
}

// ---------------------------------------------------------------------
// switch/case branch splitting
// ---------------------------------------------------------------------

fn brace_delta(line: &str) -> i64 {
    line.matches('{').count() as i64 - line.matches('}').count() as i64
}

fn is_switch_open(stripped: &str) -> bool {
    match stripped.strip_prefix("switch") {
        Some(rest) => rest.trim_start_matches(char::is_whitespace).starts_with('('),
        None => false,
    }
}

enum CaseOrDefault {
    Case(String),
    Default,
}

/// `^(case\s+([^:]+)|default)\s*:\s*(\{)?\s*$` against the WHOLE
/// `stripped` line. Returns the label text (already `.strip()`-ed; the
/// caller applies the reference's `.strip("'\"")` quote-trim).
fn match_case_label(stripped: &str) -> Option<String> {
    let (kind, after_group): (CaseOrDefault, &str) = if let Some(after_case) = stripped.strip_prefix("case") {
        let trimmed = after_case.trim_start_matches(char::is_whitespace);
        if trimmed.len() == after_case.len() {
            return None; // `case` must be followed by \s+
        }
        let colon_pos = trimmed.find(':')?;
        if colon_pos == 0 {
            return None; // `[^:]+` needs >=1 char
        }
        (
            CaseOrDefault::Case(trimmed[..colon_pos].trim().to_string()),
            &trimmed[colon_pos..],
        )
    } else if let Some(after_default) = stripped.strip_prefix("default") {
        (CaseOrDefault::Default, after_default)
    } else {
        return None;
    };
    let ws_then_colon = after_group.trim_start_matches(char::is_whitespace);
    let after_colon = ws_then_colon.strip_prefix(':')?;
    let tail = after_colon.trim_start_matches(char::is_whitespace);
    let tail = tail.strip_prefix('{').unwrap_or(tail);
    if !tail.trim().is_empty() {
        return None;
    }
    Some(match kind {
        // `.strip("'\"")` — trim any leading/trailing quote characters
        // (a `case 'a':` / `case "a":` label reads as bare `a`).
        CaseOrDefault::Case(expr) => expr.trim_matches(|c| c == '\'' || c == '"').to_string(),
        CaseOrDefault::Default => "default".to_string(),
    })
}

fn ends_with_bare_colon(line: &str) -> bool {
    let s = line.trim();
    let s = s.trim_end_matches('{');
    let s = s.trim_end();
    s.ends_with(':')
}

/// Port of `split_switch_branches`: find the first top-level `switch` in
/// `body_lines`; return `(labels, branch_lines)` per branch. A `case`/
/// `default` label with no own body (immediately followed by another
/// label, only blank lines between) merges into the FOLLOWING branch as
/// an additional label — the caller (`facts::build_param_flow_facts`)
/// reports these as `"<label> branch: fallthrough (no own block)"`.
pub fn split_switch_branches(body_lines: &[String]) -> Vec<(Vec<String>, Vec<String>)> {
    let mut depth: i64 = 0;
    let mut switch_depth: Option<i64> = None;
    let mut branches: Vec<(Vec<String>, Vec<String>)> = Vec::new();
    let mut pending_names: Vec<String> = Vec::new();
    let mut pending_start: Option<usize> = None;

    fn close(
        pending_start: Option<usize>,
        pending_names: &[String],
        end_idx: usize,
        body_lines: &[String],
        branches: &mut Vec<(Vec<String>, Vec<String>)>,
    ) {
        if let Some(ps) = pending_start {
            branches.push((pending_names.to_vec(), body_lines[ps..end_idx].to_vec()));
        }
    }

    for (idx, line) in body_lines.iter().enumerate() {
        let stripped = line.trim();
        if switch_depth.is_none() && is_switch_open(stripped) {
            depth += brace_delta(line);
            switch_depth = Some(depth);
            continue;
        }
        if switch_depth.is_none() {
            depth += brace_delta(line);
            continue;
        }
        let sd = switch_depth.unwrap();
        if let Some(label) = match_case_label(stripped) {
            if depth == sd {
                let blank_between = pending_start
                    .map(|ps| (ps + 1..idx).all(|k| body_lines[k].trim().is_empty()))
                    .unwrap_or(false);
                let merges = pending_start.is_some()
                    && blank_between
                    && !pending_names.is_empty()
                    && ends_with_bare_colon(&body_lines[pending_start.unwrap()]);
                if merges {
                    pending_names.push(label);
                    pending_start = Some(idx);
                } else {
                    close(pending_start, &pending_names, idx, body_lines, &mut branches);
                    pending_names = vec![label];
                    pending_start = Some(idx);
                }
                depth += brace_delta(line);
                continue;
            }
        }
        depth += brace_delta(line);
        if depth < sd {
            close(pending_start, &pending_names, idx, body_lines, &mut branches);
            pending_start = None;
            switch_depth = None;
        }
    }
    if switch_depth.is_some() {
        close(pending_start, &pending_names, body_lines.len(), body_lines, &mut branches);
    }
    branches
}

// ---------------------------------------------------------------------
// Identifier stem tokenization (siblings family)
// ---------------------------------------------------------------------

/// Manual camelCase/acronym tokenizer approximating
/// `[A-Z]?[a-z0-9]+|[A-Z]+(?![a-z])` (Python `re.findall`, including its
/// negative-lookahead acronym-boundary backtrack: a multi-letter
/// uppercase run whose LAST letter is immediately followed by a
/// lowercase/digit gives that last letter up to the following token,
/// e.g. `"parseURLNow"` -> `["parse", "URL", "Now"]`). No `regex` crate
/// available, so this is a direct manual re-implementation rather than a
/// literal port — behavior verified to match on typical identifiers.
///
/// Known rare edges (documented, not chased — golden stability):
/// ASCII-only classification, so non-ASCII identifier characters are
/// skipped (Python's regex would also skip them, since its pattern names
/// ASCII classes explicitly — but a unicode letter BETWEEN ASCII runs
/// splits tokens identically in both, so parity holds except for exotic
/// mixed-script names); digit-PREFIXED segments group with the lowercase
/// run they open (`[a-z0-9]+` treats digits and lowercase as one class
/// in both implementations, so `v2Parser` -> `v2` + `Parser` matches the
/// reference).
fn camel_tokens(name: &str) -> Vec<String> {
    let chars: Vec<char> = name.chars().collect();
    let n = chars.len();
    let mut tokens = Vec::new();
    let mut i = 0usize;
    let is_lower_or_digit = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    while i < n {
        let c = chars[i];
        if is_lower_or_digit(c) {
            let start = i;
            while i < n && is_lower_or_digit(chars[i]) {
                i += 1;
            }
            tokens.push(chars[start..i].iter().collect::<String>());
        } else if c.is_ascii_uppercase() {
            let mut j = i;
            while j < n && chars[j].is_ascii_uppercase() {
                j += 1;
            }
            let run_len = j - i;
            if run_len == 1 {
                if j < n && is_lower_or_digit(chars[j]) {
                    let mut k = j;
                    while k < n && is_lower_or_digit(chars[k]) {
                        k += 1;
                    }
                    tokens.push(chars[i..k].iter().collect::<String>());
                    i = k;
                } else {
                    tokens.push(chars[i..j].iter().collect::<String>());
                    i = j;
                }
            } else if j < n && is_lower_or_digit(chars[j]) {
                // The run's last uppercase letter actually starts the
                // following camelCase word — give it back (the
                // `(?![a-z])` backtrack).
                tokens.push(chars[i..j - 1].iter().collect::<String>());
                i = j - 1;
            } else {
                tokens.push(chars[i..j].iter().collect::<String>());
                i = j;
            }
        } else {
            i += 1;
        }
    }
    tokens
}

pub fn stem_tokens(name: &str) -> HashSet<String> {
    camel_tokens(name)
        .into_iter()
        .filter(|t| t.chars().count() > 2)
        .map(|t| t.to_lowercase())
        .filter(|t| !STEM_STOPLIST.contains(&t.as_str()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_simple_function_declaration() {
        let lines: Vec<String> = vec!["function foo(a, b) {", "  return a + b;", "}"]
            .into_iter()
            .map(String::from)
            .collect();
        let fns = find_all_functions_in_text(&lines);
        assert_eq!(fns.len(), 1);
        assert_eq!(fns[0].name, "foo");
        assert_eq!(fns[0].start0, 0);
        assert_eq!(fns[0].end0, 2);
    }

    #[test]
    fn finds_arrow_and_method_forms() {
        let lines: Vec<String> = vec![
            "export const bar = (x: number): number => {",
            "  return x * 2;",
            "};",
            "class Foo {",
            "  method(y) {",
            "    return y;",
            "  }",
            "}",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let fns = find_all_functions_in_text(&lines);
        let names: Vec<&str> = fns.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"bar"));
        assert!(names.contains(&"method"));
    }

    #[test]
    fn multiline_signature_is_found() {
        let lines: Vec<String> = vec![
            "function lifetimeCharge(",
            "  a: number,",
            "  b: number,",
            "): number {",
            "  return a + b;",
            "}",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let fns = find_all_functions_in_text(&lines);
        assert_eq!(fns.len(), 1);
        assert_eq!(fns[0].name, "lifetimeCharge");
        assert_eq!(fns[0].start0, 0);
        assert_eq!(fns[0].end0, 5);
    }

    #[test]
    fn call_site_argument_line_is_not_a_declaration() {
        let lines: Vec<String> = vec![
            "function caller() {",
            "  return timestampFromDateTime(",
            "    segStart,",
            "  );",
            "}",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let fns = find_all_functions_in_text(&lines);
        let names: Vec<&str> = fns.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["caller"]);
    }

    #[test]
    fn cast_expression_is_not_a_declaration() {
        let lines: Vec<String> = vec![
            "function reader() {",
            "  const value = (metrics as unknown as Record<string, number>)[metricKey];",
            "  if (value) {",
            "    return value;",
            "  }",
            "}",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let fns = find_all_functions_in_text(&lines);
        let names: Vec<&str> = fns.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["reader"]);
    }

    #[test]
    fn extract_params_skips_destructured_and_rest() {
        let lines: Vec<String> = vec!["function f(a, { b, c }, ...rest) {".to_string(), "}".to_string()];
        let params = extract_params(&lines, 0);
        assert_eq!(params, vec!["a".to_string(), "rest".to_string()]);
    }

    #[test]
    fn extract_param_defaults_finds_defaults() {
        let lines: Vec<String> =
            vec!["function f(a: number, retries = 3, timeout: number = 5000) {".to_string(), "}".to_string()];
        let defaults = extract_param_defaults(&lines, 0);
        assert_eq!(
            defaults,
            vec![
                ("retries".to_string(), "3".to_string()),
                ("timeout".to_string(), "5000".to_string()),
            ]
        );
    }

    #[test]
    fn extract_calls_finds_prefixed_and_bare_calls() {
        let calls = extract_calls("const x = obj.doThing(a, b); helper(1);");
        let names: Vec<&str> = calls.iter().map(|c| c.1.as_str()).collect();
        assert!(names.contains(&"obj.doThing"));
        assert!(names.contains(&"helper"));
    }

    #[test]
    fn extract_calls_filters_noise_and_capitalized() {
        // "if" is NOISE (a control-flow keyword); "Foo" is filtered for
        // being capitalized (constructor/class-like). "console.log" is
        // NOT noise-filtered by the reference — NOISE only excludes the
        // bare name after any `.` prefix, and "log" isn't in that list
        // (only "console" itself is, which would filter a bare
        // `console(...)` call, not `console.log(...)`).
        let calls = extract_calls("if (x) { console.log(1); Foo(1); helper(1); }");
        let names: Vec<&str> = calls.iter().map(|c| c.0.as_str()).collect();
        assert!(!names.contains(&"if"));
        assert!(!names.contains(&"Foo"));
        assert!(names.contains(&"log"));
        assert!(names.contains(&"helper"));
    }

    #[test]
    fn switch_fallthrough_labeled() {
        let body: Vec<String> = vec![
            "switch (kind) {",
            "  case 'a':",
            "  case 'b': {",
            "    doThing(x);",
            "    break;",
            "  }",
            "  default: {",
            "    other(x);",
            "  }",
            "}",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let branches = split_switch_branches(&body);
        assert_eq!(branches.len(), 2);
        assert_eq!(branches[0].0, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(branches[1].0, vec!["default".to_string()]);
    }

    #[test]
    fn ambient_label_detects_process_env_and_math_random() {
        assert_eq!(ambient_label("const x = process.env.FOO;"), Some("process.env"));
        assert_eq!(ambient_label("Math.random()"), Some("Math.random()"));
        assert_eq!(ambient_label("no ambient reads here"), None);
    }

    #[test]
    fn stem_tokens_splits_camel_case() {
        let toks = stem_tokens("parseURLNow");
        assert!(toks.contains("url"));
        assert!(toks.contains("now"));
        assert!(toks.contains("parse"));
    }

    #[test]
    fn function_at_eof_without_trailing_newline_is_found() {
        // `content.lines()` on a file with no trailing `\n` yields the same
        // Vec<String> as one WITH a trailing `\n` (Rust's `str::lines()`
        // doesn't distinguish), and `find_all_functions_in_text` rejoins
        // with `\n` (no trailing newline added) — so the real edge here is
        // whether the closing-brace scan correctly terminates when the
        // last `}` is the final byte of the joined text, with nothing
        // after it (no walk-off-the-end panic, correct end0).
        let content = "function foo(a) {\n  return a;\n}"; // no trailing \n
        let lines: Vec<String> = content.lines().map(String::from).collect();
        let fns = find_all_functions_in_text(&lines);
        assert_eq!(fns.len(), 1);
        assert_eq!(fns[0].name, "foo");
        assert_eq!(fns[0].start0, 0);
        assert_eq!(fns[0].end0, 2, "closing brace on the final line (no trailing newline) must still resolve to the last line index");
    }

    #[test]
    fn nested_switch_only_splits_outer_branches() {
        // A switch nested inside a `case` block of an outer switch must
        // NOT contribute its own case/default labels to the outer split —
        // `split_switch_branches` only ever tracks the FIRST top-level
        // switch it finds; the inner switch's braces are just ordinary
        // depth-tracked content within the outer `a` branch.
        let body: Vec<String> = vec![
            "switch (outer) {",
            "  case 'a': {",
            "    switch (inner) {",
            "      case 'x':",
            "        doX();",
            "        break;",
            "      case 'y':",
            "        doY();",
            "        break;",
            "    }",
            "    break;",
            "  }",
            "  case 'b': {",
            "    doB();",
            "    break;",
            "  }",
            "}",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let branches = split_switch_branches(&body);
        let labels: Vec<&Vec<String>> = branches.iter().map(|(names, _)| names).collect();
        assert_eq!(
            labels,
            vec![&vec!["a".to_string()], &vec!["b".to_string()]],
            "inner switch's case 'x'/'y' must not leak into the outer branch list, got: {branches:?}"
        );
        // The 'a' branch's own body must still carry the ENTIRE inner
        // switch block (its content is preserved, just not split).
        let (_, a_lines) = &branches[0];
        let a_text = a_lines.join("\n");
        assert!(a_text.contains("doX();"));
        assert!(a_text.contains("doY();"));
    }

    #[test]
    fn template_literal_with_balanced_braces_does_not_break_function_bounds() {
        // A template-literal interpolation (`${...}`) is exactly the case
        // the module docstring calls out (`Promise<{ totalRevenue: number
        // }>`-style nested braces) — but this scanner has no string/
        // template-literal awareness at all, so ANY `{`/`}` byte inside a
        // template literal (interpolated or literal text) participates in
        // the naive brace-depth count. When the literal's own braces are
        // BALANCED (equal `{` and `}` count), the net depth delta is zero
        // and the function's real closing brace is still found correctly
        // — this is the common case and must keep working.
        let lines: Vec<String> = vec![
            "function greet(name) {",
            "  const s = `hi ${name} {literal brace pair}`;",
            "  return s;",
            "}",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let fns = find_all_functions_in_text(&lines);
        assert_eq!(fns.len(), 1);
        assert_eq!(fns[0].name, "greet");
        assert_eq!(fns[0].start0, 0);
        assert_eq!(fns[0].end0, 3);
    }

    #[test]
    fn unicode_cjk_and_emoji_in_source_does_not_panic_scanner() {
        // Byte/char-boundary stress: every delimiter this scanner tests
        // for is single-byte ASCII, but multi-byte UTF-8 (CJK ideographs,
        // emoji) is free to appear inside string literals, identifiers'
        // surrounding text, and comments. The scanner must never panic on
        // a byte-offset landing mid-codepoint, and must still locate the
        // ASCII-named function correctly around the unicode content.
        let lines: Vec<String> = vec![
            "// 説明: emits a 挨拶 🎉 greeting".to_string(),
            "function 挨拶(name) {".to_string(),
            "  const msg = `こんにちは、${name}さん 🎉！`;".to_string(),
            "  helper(msg);".to_string(),
            "  return msg;".to_string(),
            "}".to_string(),
        ];
        // Must not panic across the whole scan surface this module
        // exposes over arbitrary source text.
        let fns = find_all_functions_in_text(&lines);
        let calls = extract_calls(&lines.join("\n"));
        let _ = stem_tokens("挨拶");
        let _ = count_refs(&lines.join("\n"), "msg");
        // The ASCII-identifier `helper(msg)` call must still be found
        // even with multi-byte UTF-8 immediately surrounding it on the
        // same line and in sibling lines.
        assert!(calls.iter().any(|(bare, _, _)| bare == "helper"));
        // A unicode function NAME isn't `is_ident_start_byte`-recognized
        // (ASCII-only identifier classification, documented at the top of
        // this module) — so no FnDef is expected for a non-ASCII-named
        // function, but the scan must still complete without panicking.
        assert!(fns.is_empty() || fns.iter().all(|f| f.name.is_ascii()));
    }
}
