//! (#3561) Which Node a project asks for, read from the files it already
//! has, and which pinned build answers it: Node's own reader
//! ([`super::Runtime::pick`] for [`super::node::NODE`]).
//!
//! **Where.** In each folder darkmux is given, in order (the first folder
//! with any declaration wins): `.nvmrc`, then `.node-version`, then
//! `package.json`'s `engines.node`. A project that declares nothing gets the
//! newest pin of [`DEFAULT_LINE`].
//!
//! **What darkmux reads.** An exact version (`22.23.5`, `v22.23.5`); a
//! partial one, meaning its newest release (`22`, `22.23`, `22.x`, `*`);
//! comparators (`>=20`, `<23`, `>22.1`, `<=24`, `=22.23.5`), several joined
//! by spaces (all must hold); `^` and `~`; a hyphen range (`20 - 22`); any of
//! those joined by `||`. In `.nvmrc` and `.node-version` also `lts/*` (the
//! newest pinned LTS) and `lts/<codename>` (`lts/jod`). It refuses, by name,
//! everything else: `node`, `stable`, `latest`, `current`, `system`, `iojs`,
//! a pre-release or build tag (`22.0.0-rc.1`), and any other spelling. A
//! version or range no pin satisfies is refused naming the nearest pin: only
//! a pinned build is ever fetched.
//!
//! **Untrusted files.** The workspace is written by a run, so a declaration
//! is read only from a regular file in the folder itself (a link is refused,
//! never followed), at most 4 KiB for a version file and 1 MiB for
//! `package.json`. A `package.json` that does not parse as JSON declares
//! nothing here (npm itself refuses it); one whose `engines.node` is not a
//! string is refused.

use super::node::DEFAULT_LINE;
use super::pins::{Pin, Version};
use std::path::{Path, PathBuf};

/// Where a project declares its Node version, in the order darkmux reads
/// them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Declared {
    Nvmrc,
    NodeVersion,
    Engines,
}

impl Declared {
    /// The file it is in.
    pub fn file(self) -> &'static str {
        match self {
            Declared::Nvmrc => ".nvmrc",
            Declared::NodeVersion => ".node-version",
            Declared::Engines => "package.json",
        }
    }

    /// How a message names it.
    fn named(self) -> &'static str {
        match self {
            Declared::Nvmrc => "`.nvmrc`",
            Declared::NodeVersion => "`.node-version`",
            Declared::Engines => "`package.json`'s `engines.node`",
        }
    }
}

/// A project's declaration: where, the folder it is in, and its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub from: Declared,
    pub dir: PathBuf,
    pub text: String,
}

const MAX_VERSION_FILE: u64 = 4 * 1024;
const MAX_PACKAGE_JSON: u64 = 1024 * 1024;

/// The first declaration in `dirs`, in order: in each folder `.nvmrc`, then
/// `.node-version`, then `package.json`'s `engines.node`. `None` when no
/// folder declares one; `Err` for a declaration darkmux will not read (a
/// link, not a regular file, too large, not text).
pub fn declared_in(dirs: &[&Path]) -> Result<Option<Request>, String> {
    for dir in dirs {
        for from in [Declared::Nvmrc, Declared::NodeVersion] {
            if let Some(text) = version_file(dir, from)? {
                return Ok(Some(Request {
                    from,
                    dir: dir.to_path_buf(),
                    text,
                }));
            }
        }
        if let Some(text) = engines_node(dir)? {
            return Ok(Some(Request {
                from: Declared::Engines,
                dir: dir.to_path_buf(),
                text,
            }));
        }
    }
    Ok(None)
}

/// `dir`'s `from` file as text, when it is there: a regular file (never
/// followed through a link) of at most `max` bytes.
fn read_declaration(dir: &Path, from: Declared, max: u64) -> Result<Option<String>, String> {
    let path = dir.join(from.file());
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("reading {}: {e}", path.display())),
    };
    if !meta.file_type().is_file() {
        return Err(format!(
            "{} is not a regular file (a link or a folder); darkmux reads a project's Node version only from a \
             regular file in it, so it never reads through a link. Replace it with the file itself",
            path.display()
        ));
    }
    if meta.len() > max {
        return Err(format!(
            "{} is larger than {max} bytes; darkmux does not read a Node version from it",
            path.display()
        ));
    }
    // (#3561 review) A leading UTF-8 byte order mark is not part of the
    // declaration (npm strips it from `package.json` too).
    std::fs::read_to_string(&path)
        .map(|t| Some(t.strip_prefix('\u{feff}').map(str::to_string).unwrap_or(t)))
        .map_err(|e| format!("reading {}: {e}", path.display()))
}

/// The version an `.nvmrc` or `.node-version` names: its first line that is
/// not blank, with a `#` comment cut off. `None` for a file with none.
fn version_file(dir: &Path, from: Declared) -> Result<Option<String>, String> {
    let Some(text) = read_declaration(dir, from, MAX_VERSION_FILE)? else {
        return Ok(None);
    };
    Ok(text
        .lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .find(|l| !l.is_empty())
        .map(str::to_string))
}

/// `package.json`'s `engines.node`, when it has one.
fn engines_node(dir: &Path) -> Result<Option<String>, String> {
    let Some(text) = read_declaration(dir, Declared::Engines, MAX_PACKAGE_JSON)? else {
        return Ok(None);
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Ok(None);
    };
    match json.get("engines").and_then(|e| e.get("node")) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s.trim().to_string())),
        Some(other) => Err(format!(
            "{}'s engines.node is {other}, not a string; darkmux reads a version or a range there (`22`, `>=20`)",
            dir.join("package.json").display()
        )),
    }
}

/// A version with any part left open (`22`, `22.x`, `*`): `None` is a
/// wildcard, and every part after one is a wildcard too.
#[derive(Debug, Clone, Copy)]
struct Partial {
    major: Option<u32>,
    minor: Option<u32>,
    patch: Option<u32>,
}

impl Partial {
    /// Every part named: an exact version.
    fn exact(self) -> Option<Version> {
        Some(Version::new(self.major?, self.minor?, self.patch?))
    }

    /// The lowest version it allows: its open parts at 0.
    fn floor(self) -> Version {
        Version::new(
            self.major.unwrap_or(0),
            self.minor.unwrap_or(0),
            self.patch.unwrap_or(0),
        )
    }

    /// The first version past every one it allows, for a partial one
    /// (`22` -> `23.0.0`, `22.4` -> `22.5.0`); `None` for an exact version
    /// or a bare wildcard, which have no such bound. [`Overflow`] when that
    /// version is past what a version part holds.
    fn ceiling(self) -> Result<Option<Version>, Overflow> {
        Ok(match (self.major, self.minor, self.patch) {
            (Some(m), None, _) => Some(Version::new(up(m)?, 0, 0)),
            (Some(m), Some(n), None) => Some(Version::new(m, up(n)?, 0)),
            _ => None,
        })
    }
}

/// (#3561 review) A bound past the largest version part (`~4294967295`):
/// such a request is refused as unreadable, never wrapped around.
#[derive(Debug)]
struct Overflow;

/// `n + 1`, or [`Overflow`].
fn up(n: u32) -> Result<u32, Overflow> {
    n.checked_add(1).ok_or(Overflow)
}

/// `s` as a partial version (an optional leading `v`), or `None` for any
/// other spelling (a pre-release tag, a word).
fn partial(s: &str) -> Option<Partial> {
    let s = s.strip_prefix(['v', 'V']).unwrap_or(s);
    let parts: Vec<&str> = if s.is_empty() {
        Vec::new()
    } else {
        s.split('.').collect()
    };
    if parts.len() > 3 {
        return None;
    }
    let mut out = [None; 3];
    let mut open = false;
    for (i, part) in parts.iter().enumerate() {
        open |= is_wildcard(part);
        if !is_wildcard(part) {
            out[i] = Some(number(part).filter(|_| !open)?);
        }
    }
    Some(Partial {
        major: out[0],
        minor: out[1],
        patch: out[2],
    })
}

/// A part that leaves its place open: `x`, `X` or `*`.
fn is_wildcard(part: &str) -> bool {
    matches!(part, "x" | "X" | "*")
}

/// A part that is a plain number (digits only, no sign or space).
fn number(part: &str) -> Option<u32> {
    (!part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        .then(|| part.parse().ok())
        .flatten()
}

/// One end of an interval.
#[derive(Debug, Clone, Copy)]
struct Bound {
    v: Version,
    inclusive: bool,
}

/// The versions between `lo` and `hi`; an open end is unbounded.
#[derive(Debug, Clone, Copy, Default)]
struct Interval {
    lo: Option<Bound>,
    hi: Option<Bound>,
}

/// An interval nothing is in.
const NOTHING: Interval = Interval {
    lo: Some(Bound {
        v: Version::new(u32::MAX, u32::MAX, u32::MAX),
        inclusive: false,
    }),
    hi: None,
};

impl Interval {
    fn at_least(v: Version) -> Self {
        Interval {
            lo: Some(Bound { v, inclusive: true }),
            hi: None,
        }
    }

    fn below(self, v: Version, inclusive: bool) -> Self {
        Interval {
            hi: Some(Bound { v, inclusive }),
            ..self
        }
    }

    fn contains(&self, v: Version) -> bool {
        let above = self
            .lo
            .is_none_or(|b| if b.inclusive { v >= b.v } else { v > b.v });
        let under = self
            .hi
            .is_none_or(|b| if b.inclusive { v <= b.v } else { v < b.v });
        above && under
    }

    /// Both intervals at once.
    fn and(self, other: Interval) -> Interval {
        let tighter_lo = |a: Option<Bound>, b: Option<Bound>| match (a, b) {
            (Some(a), Some(b)) if (b.v, !b.inclusive) > (a.v, !a.inclusive) => Some(b),
            (Some(a), _) => Some(a),
            (None, b) => b,
        };
        let tighter_hi = |a: Option<Bound>, b: Option<Bound>| match (a, b) {
            (Some(a), Some(b)) if (b.v, b.inclusive) < (a.v, a.inclusive) => Some(b),
            (Some(a), _) => Some(a),
            (None, b) => b,
        };
        Interval {
            lo: tighter_lo(self.lo, other.lo),
            hi: tighter_hi(self.hi, other.hi),
        }
    }
}

/// A bare or `=` partial version: an exact one, or every release it leaves
/// open.
fn x_range(p: Partial) -> Result<Interval, Overflow> {
    Ok(match (p.exact(), p.ceiling()?) {
        (Some(v), _) => Interval::at_least(v).below(v, true),
        (None, Some(c)) => Interval::at_least(p.floor()).below(c, false),
        (None, None) => Interval::default(),
    })
}

/// `^p`: changes that keep its first nonzero part.
fn caret(p: Partial) -> Result<Interval, Overflow> {
    let Some(major) = p.major else {
        return Ok(Interval::default());
    };
    let hi = match (major, p.minor, p.patch) {
        (0, None, _) => Version::new(1, 0, 0),
        (0, Some(0), None) => Version::new(0, 1, 0),
        (0, Some(0), Some(z)) => Version::new(0, 0, up(z)?),
        (0, Some(n), _) => Version::new(0, up(n)?, 0),
        (m, _, _) => Version::new(up(m)?, 0, 0),
    };
    Ok(Interval::at_least(p.floor()).below(hi, false))
}

/// `~p`: patch releases, or minor ones when only a major is named.
fn tilde(p: Partial) -> Result<Interval, Overflow> {
    let lo = Interval::at_least(p.floor());
    Ok(match (p.major, p.minor) {
        (None, _) => Interval::default(),
        (Some(m), None) => lo.below(Version::new(up(m)?, 0, 0), false),
        (Some(m), Some(n)) => lo.below(Version::new(m, up(n)?, 0), false),
    })
}

/// The operators a comparator may start with, longest first.
const OPERATORS: [&str; 8] = [">=", "<=", "~>", ">", "<", "=", "^", "~"];

/// One comparator (`>=20`, `^22.1`, `22.x`), or `None` for a spelling
/// darkmux does not read (or a bound past the largest version part).
fn comparator(token: &str) -> Option<Interval> {
    let (op, rest) = OPERATORS
        .iter()
        .find_map(|op| token.strip_prefix(op).map(|r| (*op, r)))
        .unwrap_or(("", token));
    let p = partial(rest)?;
    match op {
        "" | "=" => x_range(p).ok(),
        "^" => caret(p).ok(),
        "~" | "~>" => tilde(p).ok(),
        ">=" => Some(Interval::at_least(p.floor())),
        ">" => above(p),
        "<" => Some(under(p)),
        "<=" => at_most(p),
        _ => None,
    }
}

/// `<p`: before its lowest release; nothing is before `*`.
fn under(p: Partial) -> Interval {
    if p.major.is_none() {
        NOTHING
    } else {
        Interval::default().below(p.floor(), false)
    }
}

/// `>p`: past an exact version, or past every release a partial one
/// allows; nothing is past `*`.
fn above(p: Partial) -> Option<Interval> {
    if p.major.is_none() {
        return Some(NOTHING);
    }
    Some(match (p.exact(), p.ceiling().ok()?) {
        (Some(v), _) => Interval {
            lo: Some(Bound {
                v,
                inclusive: false,
            }),
            hi: None,
        },
        (None, c) => Interval::at_least(c.unwrap_or(p.floor())),
    })
}

/// `<=p`, and a hyphen range's upper end: up to an exact version, or every
/// release a partial one allows.
fn at_most(p: Partial) -> Option<Interval> {
    Some(match (p.exact(), p.ceiling().ok()?) {
        (Some(v), _) => Interval::default().below(v, true),
        (None, Some(c)) => Interval::default().below(c, false),
        (None, None) => Interval::default(),
    })
}

/// One `||`-free set: a hyphen range, or comparators that must all hold
/// (an operator may stand apart from its version: `>= 20`).
fn comparator_set(set: &str) -> Option<Interval> {
    let words: Vec<&str> = set.split_whitespace().collect();
    if let [a, "-", b] = words[..] {
        return Some(Interval::at_least(partial(a)?.floor()).and(at_most(partial(b)?)?));
    }
    joined(&words)?
        .iter()
        .try_fold(Interval::default(), |acc, t| Some(acc.and(comparator(t)?)))
}

/// `words` with each operator standing apart joined to the version after
/// it (`>=`, `20` -> `>=20`); `None` for two operators in a row or one at
/// the end.
fn joined(words: &[&str]) -> Option<Vec<String>> {
    let mut tokens: Vec<String> = Vec::new();
    let mut pending: Option<&str> = None;
    for w in words {
        let operator = OPERATORS.contains(w);
        if operator && pending.is_some() {
            return None;
        }
        if operator {
            pending = Some(w);
        } else {
            tokens.push(format!("{}{w}", pending.take().unwrap_or("")));
        }
    }
    pending.is_none().then_some(tokens)
}

/// A whole range, its `||` alternatives each an interval.
fn range(text: &str) -> Option<Vec<Interval>> {
    text.split("||").map(comparator_set).collect()
}

/// The words nvm reads as an alias that darkmux pins nothing for.
const ALIASES: &[&str] = &[
    "node", "stable", "latest", "current", "system", "iojs", "unstable",
];

/// What darkmux reads, for a message refusing anything else.
const READABLE: &str = "an exact version (`22.23.5`), a partial one meaning its newest release (`22`, `22.23`, \
     `22.x`, `*`), comparators (`>=20`, `<23`), `^`, `~`, a hyphen range (`20 - 22`) and `||`, and in `.nvmrc` or \
     `.node-version` also `lts/*` and `lts/<codename>`";

/// The newest of `pins` (never empty) that `keep` allows.
fn newest(pins: &[Pin], keep: impl Fn(&Pin) -> bool) -> Option<&Pin> {
    pins.iter().filter(|p| keep(p)).max_by_key(|p| p.version)
}

/// The pin nearest `target`: its own line's newest, else the nearest line's
/// (the newer of two equally near).
fn nearest(pins: &[Pin], target: Version) -> &Pin {
    pins.iter()
        .min_by_key(|p| {
            (
                p.version.major.abs_diff(target.major),
                std::cmp::Reverse(p.version),
            )
        })
        .expect("a platform with no pins is refused before a version is chosen")
}

/// Every pinned version, newest first, for a message.
fn pinned_list(pins: &[Pin]) -> String {
    let mut versions: Vec<Version> = pins.iter().map(|p| p.version).collect();
    versions.sort();
    versions.dedup();
    versions
        .iter()
        .rev()
        .map(Version::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The pinned build of `pins` (this platform's, never empty) that
/// `request` asks for: with none, the newest of [`DEFAULT_LINE`].
pub fn choose<'p>(request: Option<&Request>, pins: &'p [Pin]) -> Result<&'p Pin, String> {
    let Some(r) = request else {
        return Ok(newest(pins, |p| p.version.major == DEFAULT_LINE)
            .or_else(|| newest(pins, |_| true))
            .expect("pins"));
    };
    let text = r.text.trim();
    let lower = text.to_ascii_lowercase();
    let at = r.dir.join(r.from.file());
    if let Some(line) = lower
        .strip_prefix("lts/")
        .filter(|_| r.from != Declared::Engines)
    {
        return lts(r, line, pins);
    }
    if ALIASES.contains(&lower.as_str()) || lower.starts_with("lts") || range(text).is_none() {
        return Err(format!(
            "{} in {} asks for Node `{text}`, which darkmux cannot read as a version it pins. It reads {READABLE}; \
             it refuses `node`, `latest`, `current`, `system` and pre-release versions. Write one of those, for \
             example `{}`",
            r.from.named(),
            at.display(),
            nearest(pins, Version::new(DEFAULT_LINE, 0, 0)).version.major,
        ));
    }
    let sets = range(text).unwrap_or_default();
    if let Some(pin) = newest(pins, |p| sets.iter().any(|s| s.contains(p.version))) {
        return Ok(pin);
    }
    let target = sets
        .iter()
        .find_map(|s| s.lo.or(s.hi))
        .map_or(Version::new(DEFAULT_LINE, 0, 0), |b| b.v);
    let near = nearest(pins, target);
    Err(format!(
        "{} in {} asks for Node `{text}`, and no version darkmux pins satisfies it: darkmux fetches only versions \
         whose SHA-256 is pinned in its binary ({}). The nearest is {}: set it to `{}` (or `{}`)",
        r.from.named(),
        at.display(),
        pinned_list(pins),
        near.version,
        near.version,
        near.version.major,
    ))
}

/// `lts/*` or `lts/<codename>`.
fn lts<'p>(r: &Request, line: &str, pins: &'p [Pin]) -> Result<&'p Pin, String> {
    let found = if line == "*" {
        newest(pins, |_| true)
    } else {
        newest(pins, |p| p.line == line)
    };
    found.ok_or_else(|| {
        let mut names: Vec<&str> = pins.iter().map(|p| p.line).collect();
        names.dedup();
        format!(
            "{} in {} asks for `lts/{line}`, an LTS line darkmux pins no build of; it pins `lts/*` and {}",
            r.from.named(),
            r.dir.join(r.from.file()).display(),
            names.iter().map(|n| format!("`lts/{n}`")).collect::<Vec<_>>().join(", "),
        )
    })
}
