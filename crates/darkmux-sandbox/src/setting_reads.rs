//! Which functions read `runtime.sandbox`, derived from this crate's own
//! source, and where a piece of source calls one. For the guard tests of
//! code that reads the setting once and carries the value through a run: a
//! list written by hand misses the next helper here that reads it, so the
//! list is computed from what each function's body calls.

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

/// The accessors a file may import and call bare.
const ACCESSOR_NAMES: [&str; 2] = ["sandbox_with_source", "sandbox_with_source_in"];

/// One function definition: its name, whether it is `pub`, and its body.
struct Def<'a> {
    name: &'a str,
    public: bool,
    body: String,
}

/// Every public function of this crate that reads the setting, directly
/// (a types-crate accessor) or through another function of this crate,
/// sorted.
pub fn readers() -> Vec<String> {
    let defs: Vec<Def> = SOURCES.iter().flat_map(|src| definitions(production(src))).collect();
    let mut reading: Vec<&str> = defs
        .iter()
        .filter(|d| ACCESSOR_PATHS.iter().any(|a| d.body.contains(a)))
        .map(|d| d.name)
        .collect();
    loop {
        let before = reading.len();
        for d in &defs {
            if !reading.contains(&d.name) && !calls(&d.body, &reading, &["crate::", "Self::", "super::"]).is_empty() {
                reading.push(d.name);
            }
        }
        if reading.len() == before {
            break;
        }
    }
    let mut out: Vec<String> = defs
        .iter()
        .filter(|d| d.public && reading.contains(&d.name))
        .map(|d| d.name.to_string())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The byte offsets in `src` (production code, comments ignored) where the
/// setting is read: a call of one of [`readers`] or of `extra` (a caller's
/// own reading functions), as `darkmux_sandbox::name(` or imported bare as
/// `name(`; a rename of one on import (`name as`); or a types-crate
/// accessor. A method call (`.name(`) and a definition (`fn name(`) are not
/// reads. Each hit is named.
pub fn reads_in(src: &str, extra: &[&str]) -> Vec<(usize, String)> {
    let text = without_comments(src);
    let derived = readers();
    let names: Vec<&str> = derived.iter().map(String::as_str).chain(extra.iter().copied()).chain(ACCESSOR_NAMES).collect();
    let mut hits = calls(&text, &names, &["darkmux_sandbox::"]);
    for a in ACCESSOR_PATHS {
        hits.extend(text.match_indices(a).map(|(i, _)| (i, a.trim_end_matches('(').to_string())));
    }
    for name in &names {
        for (i, _) in text.match_indices(&format!("{name} as ")) {
            if !ident_before(&text, i) {
                hits.push((i, format!("{name} as")));
            }
        }
    }
    hits.sort();
    hits.dedup();
    hits
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
        if ident_before(src, at) {
            continue;
        }
        let rest = &src[at + 3..];
        let len = rest.find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(rest.len());
        if len == 0 || !matches!(rest[len..].chars().next(), Some('(') | Some('<')) {
            continue;
        }
        let name = &rest[..len];
        let line_start = src[..at].rfind('\n').map_or(0, |i| i + 1);
        let public = src[line_start..at].trim_start().starts_with("pub ");
        let open = text[at..].find('{').map(|i| at + i);
        let semi = text[at..].find(';').map(|i| at + i);
        let Some(open) = open.filter(|o| semi.is_none_or(|s| *o < s)) else { continue };
        let mut depth = 0usize;
        let mut end = text.len();
        for (i, c) in text[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = open + i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        out.push(Def { name, public, body: text[open..end].to_string() });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_readers_are_derived_from_what_each_function_calls() {
        let r = readers();
        for reads in [
            "selected",
            "selection",
            "selected_in",
            "backend",
            "docker_remedy",
            "docker_needs",
            "placed_in_configured",
            "seen_at_configured",
            "way_back_to_docker",
        ] {
            assert!(r.iter().any(|n| n == reads), "{reads} reads the setting: {r:?}");
        }
        for pure in ["placed_in", "seen_at", "backend_of", "setting_value", "way_back_to_docker_given", "for_handle"] {
            assert!(!r.iter().any(|n| n == pure), "{pure} takes its value: {r:?}");
        }
    }

    #[test]
    fn a_read_is_found_however_it_is_spelled_and_nothing_else_is() {
        let names = |src: &str| reads_in(src, &["my_reader"]).into_iter().map(|(_, n)| n).collect::<Vec<_>>();
        assert_eq!(names("let k = darkmux_sandbox::selected();"), ["selected"]);
        assert_eq!(names("use darkmux_sandbox::selected;\nfn f() { selected(); }"), ["selected"]);
        assert_eq!(names("use darkmux_sandbox::{docker_remedy as r};"), ["docker_remedy as"]);
        assert_eq!(names("let r = darkmux_sandbox::docker_remedy();"), ["docker_remedy"]);
        assert_eq!(names("let x = darkmux_types::config_access::sandbox_with_source();"), ["config_access::sandbox_with_source"]);
        assert_eq!(names("use darkmux_types::config_access::sandbox_with_source;\nlet x = sandbox_with_source();"), ["sandbox_with_source"]);
        assert_eq!(names("x = my_reader(1);"), ["my_reader"]);
        assert!(names("let r = sel.docker_remedy(); let k = sel.kind;").is_empty(), "a method on a carried value");
        assert!(names("fn selected() {}\n// selected() in a comment").is_empty());
        assert!(names("let s = \"// not a comment\"; darkmux_sandbox::backend_of(k);").is_empty());
        assert!(names("other::selected();").is_empty(), "another path's function of the same name");
    }
}
