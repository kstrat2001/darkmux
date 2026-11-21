//! Which public functions of this crate read `runtime.sandbox`, derived
//! from the crate's own source: a function that calls a types-crate
//! accessor of the setting, or another function here that does. The
//! crate's reading API is exactly [`crate::selection`] and
//! [`crate::selection_in`] (asserted below), so the workspace guard
//! (`tests/sandbox_setting_reads.rs`) has two names to look for here, and a
//! new public function that reads the setting fails this crate's own test.

/// This crate's sources (its test modules are cut off where they start).
const SOURCES: [&str; 7] = [
    include_str!("lib.rs"),
    include_str!("docker.rs"),
    include_str!("docker_calls.rs"),
    include_str!("host_leaders.rs"),
    include_str!("landlock.rs"),
    include_str!("seatbelt.rs"),
    include_str!("toolchain.rs"),
];

/// The types crate's accessors of the setting, as source spells them.
const ACCESSOR_PATHS: [&str; 3] = [
    "config_access::sandbox(",
    "config_access::sandbox_with_source(",
    "config_access::sandbox_with_source_in(",
];

/// One function definition: its name, whether it is `pub`, and its body.
struct Def<'a> {
    name: &'a str,
    public: bool,
    body: String,
}

/// Every public function of this crate that reads the setting, directly
/// (a types-crate accessor) or through another function of this crate,
/// sorted.
fn readers() -> Vec<String> {
    let defs: Vec<Def> = SOURCES.iter().flat_map(|src| definitions(production(src))).collect();
    let mut reading: Vec<&str> = defs
        .iter()
        .filter(|d| ACCESSOR_PATHS.iter().any(|a| d.body.contains(a)))
        .map(|d| d.name)
        .collect();
    close_over(&defs, &mut reading);
    let mut out: Vec<String> = defs
        .iter()
        .filter(|d| d.public && reading.contains(&d.name))
        .map(|d| d.name.to_string())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Add to `reading` every function of `defs` that calls one in it, until
/// none is added.
fn close_over<'a>(defs: &[Def<'a>], reading: &mut Vec<&'a str>) {
    loop {
        let before = reading.len();
        for d in defs {
            if !reading.contains(&d.name) && !calls(&d.body, reading, &["crate::", "Self::", "super::"]).is_empty() {
                reading.push(d.name);
            }
        }
        if reading.len() == before {
            break;
        }
    }
}

/// The calls in `text` of any of `names`, bare or through one of `paths`.
fn calls(text: &str, names: &[&str], paths: &[&str]) -> Vec<(usize, String)> {
    let mut hits = Vec::new();
    for name in names {
        for (i, _) in text.match_indices(&format!("{name}(")) {
            if ident_before(text, i) || text[..i].ends_with('.') || text[..i].ends_with("fn ") {
                continue;
            }
            let bare = !text[..i].ends_with("::");
            if bare || paths.iter().any(|p| text[..i].ends_with(p)) {
                hits.push((i, (*name).to_string()));
            }
        }
    }
    hits
}

fn ident_before(text: &str, i: usize) -> bool {
    text[..i].chars().next_back().is_some_and(|c| c.is_alphanumeric() || c == '_')
}

/// `src` up to its test module.
fn production(src: &str) -> &str {
    &src[..src.find("\n#[cfg(test)]\nmod tests").unwrap_or(src.len())]
}

/// `src` with every `//` comment blanked (a `//` inside a string literal,
/// an odd number of quotes before it on its line, is kept).
fn without_comments(src: &str) -> String {
    src.lines()
        .map(|line| {
            let cut = line.match_indices("//").map(|(i, _)| i).find(|&i| line[..i].matches('"').count() % 2 == 0);
            match cut {
                Some(i) => format!("{}{}", &line[..i], " ".repeat(line.len() - i)),
                None => line.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The functions defined in `src`, with their bodies (comments blanked).
fn definitions(src: &str) -> Vec<Def<'_>> {
    let text = without_comments(src);
    let mut out = Vec::new();
    for (at, _) in src.match_indices("fn ") {
        let Some(name) = fn_name_at(src, at) else { continue };
        let line_start = src[..at].rfind('\n').map_or(0, |i| i + 1);
        let public = src[line_start..at].trim_start().starts_with("pub ");
        let open = text[at..].find('{').map(|i| at + i);
        let semi = text[at..].find(';').map(|i| at + i);
        let Some(open) = open.filter(|o| semi.is_none_or(|s| *o < s)) else { continue };
        out.push(Def { name, public, body: text[open..block_end(&text, open)].to_string() });
    }
    out
}

/// The name of the function `fn ` at `at` defines, or `None` when the
/// `fn` there is part of another word or names no function.
fn fn_name_at(src: &str, at: usize) -> Option<&str> {
    if ident_before(src, at) {
        return None;
    }
    let rest = &src[at + 3..];
    let len = rest.find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(rest.len());
    (len > 0 && matches!(rest[len..].chars().next(), Some('(') | Some('<'))).then(|| &rest[..len])
}

/// The end of the block that opens at `open` (one past its `}`).
fn block_end(text: &str, open: usize) -> usize {
    let mut depth = 0usize;
    for (i, c) in text[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return open + i + 1;
                }
            }
            _ => {}
        }
    }
    text.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The crate's reading API is these two functions; every other public
    /// function takes the value. A new one that reads the setting, directly
    /// or through another function here, fails this.
    #[test]
    fn the_only_public_readers_of_the_setting_are_selection_and_selection_in() {
        assert_eq!(readers(), ["selection", "selection_in"]);
    }

    /// The derivation follows calls through other functions, ignores method
    /// calls on a carried value, and finds nothing in a function that only
    /// takes the value.
    #[test]
    fn a_reader_is_found_through_the_functions_it_calls() {
        let src = "pub fn a() { config_access::sandbox_with_source(); }\n\
                   fn b() { a(); }\n\
                   pub fn c() { b(); }\n\
                   pub fn d(s: Sel) { s.c(); }\n\
                   pub fn e(k: Kind) -> Kind { k }\n";
        let defs = definitions(src);
        let mut reading: Vec<&str> = defs.iter().filter(|d| ACCESSOR_PATHS.iter().any(|a| d.body.contains(a))).map(|d| d.name).collect();
        close_over(&defs, &mut reading);
        reading.sort();
        assert_eq!(reading, ["a", "b", "c"]);
    }
}
