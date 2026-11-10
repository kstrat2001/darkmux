//! A hand edit's text match (#3533): where the words a person selected in a
//! rendered page sit in a source file.
//!
//! Rendered text is not source text. A page shows one space where the source
//! wraps a line and indents the next, and shows `&` where the source holds
//! `&amp;`. So both sides are compared with every whitespace run read as one
//! space and the common HTML entities decoded on the source side; the match is
//! the source span those words came from, so a replacement keeps the rest of
//! the file exactly as it was. Text split by inline markup (`**bold**`, a
//! `<strong>`) is not found, and the app falls back to a Coder run for it.

/// The entities a page decodes that a source file commonly holds.
const ENTITIES: &[(&str, char)] = &[
    ("&amp;", '&'),
    ("&lt;", '<'),
    ("&gt;", '>'),
    ("&quot;", '"'),
    ("&#39;", '\''),
    ("&apos;", '\''),
    ("&nbsp;", ' '),
    ("&#123;", '{'),
    ("&#125;", '}'),
];

/// The file types whose source a page shows decoded, where `&`, `<`, `>`,
/// `{` or `}` typed as text would read as markup or code: HTML, the
/// templates and JSX built on it, and Markdown, which passes HTML through.
const MARKUP: &[&str] = &["html", "htm", "vue", "svelte", "astro", "jsx", "tsx", "mdx", "md", "markdown"];

/// `new`, the text a page is to show, as source for `file`. In a markup
/// file ([`MARKUP`]) the characters that would read as markup or code are
/// written as entities every one of those types decodes (and [`find`]
/// reads back); quotes stay as typed, since in text they are only text.
/// In any other file it is written as sent.
pub fn as_source(file: &str, new: &str) -> String {
    let ext = file.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    if !MARKUP.contains(&ext.as_str()) {
        return new.to_string();
    }
    let mut out = String::with_capacity(new.len());
    for c in new.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '{' => out.push_str("&#123;"),
            '}' => out.push_str("&#125;"),
            c => out.push(c),
        }
    }
    out
}

/// The text as the page reads it: whitespace runs (no-break spaces
/// included) as one space, trimmed. Empty when it holds no words.
pub fn normalize(text: &str) -> String {
    text.split(|c: char| c.is_whitespace() || c == '\u{a0}')
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `source` read the way a page shows it, each char with the byte span of
/// the source it came from.
fn rendered(source: &str) -> Vec<(char, usize, usize)> {
    let mut out = Vec::new();
    // A `for` over the chars, skipping what a run or an entity already
    // took: no index arithmetic can keep it from ending (#3533's mutation
    // shard timed out on mutants that turned a `while` into a spin).
    let mut taken = 0;
    for (i, c) in source.char_indices() {
        if i < taken {
            continue;
        }
        let rest = &source[i..];
        if c.is_whitespace() || c == '\u{a0}' {
            let run = rest
                .char_indices()
                .find(|(_, c)| !(c.is_whitespace() || *c == '\u{a0}'))
                .map_or(rest.len(), |(n, _)| n);
            out.push((' ', i, i + run));
            taken = i + run;
        } else if let Some((ent, ch)) = ENTITIES.iter().find(|(e, _)| rest.starts_with(e)) {
            // A no-break space entity is whitespace too, and joins a run.
            if *ch == ' ' && matches!(out.last(), Some((' ', _, end)) if *end == i) {
                out.last_mut().expect("just matched").2 = i + ent.len();
            } else {
                out.push((*ch, i, i + ent.len()));
            }
            taken = i + ent.len();
        } else {
            out.push((c, i, i + c.len_utf8()));
        }
    }
    out
}

/// The byte spans in `source` whose rendered text is `wanted` (already
/// [`normalize`]d), without overlap, at most `limit` of them.
pub fn find(source: &str, wanted: &str, limit: usize) -> Vec<(usize, usize)> {
    let want: Vec<char> = wanted.chars().collect();
    if want.is_empty() {
        return Vec::new();
    }
    let page = rendered(source);
    let mut found = Vec::new();
    // Bounded like `rendered`: a match skips its own chars through `next`.
    let mut next = 0;
    for at in 0..page.len() {
        if found.len() >= limit {
            break;
        }
        let end = at + want.len();
        if at < next || end > page.len() {
            continue;
        }
        if matches_at(&page, &want, at) {
            found.push((page[at].1, page[end - 1].2));
            next = end;
        }
    }
    found
}

/// Whether `want` is the whole words of `page` starting at `at`.
fn matches_at(page: &[(char, usize, usize)], want: &[char], at: usize) -> bool {
    let end = at + want.len();
    // Whole words only: "more" is not found inside "furthermore".
    let alnum = |i: Option<usize>| {
        i.and_then(|i| page.get(i))
            .is_some_and(|(c, _, _)| c.is_alphanumeric())
    };
    let cut_word = (want[0].is_alphanumeric() && alnum(at.checked_sub(1)))
        || (want[want.len() - 1].is_alphanumeric() && alnum(Some(end)));
    !cut_word
        && page[at..end]
            .iter()
            .map(|(c, _, _)| *c)
            .eq(want.iter().copied())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(source: &str, wanted: &str) -> Option<String> {
        let hits = find(source, &normalize(wanted), 2);
        (hits.len() == 1).then(|| source[hits[0].0..hits[0].1].to_string())
    }

    #[test]
    fn plain_text_matches_itself() {
        assert_eq!(
            one("<h1>Welcome home</h1>", "Welcome home").as_deref(),
            Some("Welcome home")
        );
    }

    #[test]
    fn a_wrapped_and_indented_line_reads_as_one_space() {
        let src = "<p>\n      Build something\n      people love.\n    </p>";
        assert_eq!(
            one(src, "Build something people love.").as_deref(),
            Some("Build something\n      people love.")
        );
    }

    #[test]
    fn entities_in_the_source_match_the_characters_the_page_shows() {
        assert_eq!(
            one("<p>Tea &amp; cake</p>", "Tea & cake").as_deref(),
            Some("Tea &amp; cake")
        );
        assert_eq!(
            one("<p>Hi&nbsp;there</p>", "Hi\u{a0}there").as_deref(),
            Some("Hi&nbsp;there")
        );
    }

    #[test]
    fn selection_whitespace_is_trimmed_and_collapsed() {
        assert_eq!(normalize("  Hello \n\t world  "), "Hello world");
        assert_eq!(normalize(" \n "), "");
    }

    #[test]
    fn every_occurrence_is_counted_up_to_the_limit() {
        let src = "Learn more. Learn more. Learn more.";
        assert_eq!(find(src, "Learn more", 2).len(), 2);
        assert_eq!(find(src, "Learn more", 10).len(), 3);
    }

    #[test]
    fn a_word_inside_a_longer_word_is_not_a_match() {
        assert_eq!(
            one("Furthermore, read more", "more").as_deref(),
            Some("more")
        );
    }

    #[test]
    fn text_split_by_markup_is_not_found() {
        assert_eq!(one("It is **really** good", "It is really good"), None);
    }

    #[test]
    fn typed_text_is_written_as_text_in_markup_files_only() {
        let typed = "Tea <b>&</b> {cake} \"it's\"";
        let escaped = "Tea &lt;b&gt;&amp;&lt;/b&gt; &#123;cake&#125; \"it's\"";
        for file in ["a.html", "b/c.HTM", "x.vue", "x.svelte", "x.astro", "App.jsx", "App.tsx", "p.mdx", "README.md", "n.markdown"] {
            assert_eq!(as_source(file, typed), escaped, "{file}");
        }
        for file in ["a.txt", "app.js", "app.ts", "s.css", "d.json", "Makefile", "html"] {
            assert_eq!(as_source(file, typed), typed, "{file}");
        }
    }

    #[test]
    fn escaped_text_is_found_by_what_the_page_shows() {
        let src = format!("<p>{}</p>", as_source("a.html", "{a} & <b>"));
        assert_eq!(one(&src, "{a} & <b>").as_deref(), Some("&#123;a&#125; &amp; &lt;b&gt;"));
    }

    #[test]
    fn non_ascii_text_matches_on_char_boundaries() {
        assert_eq!(
            one("<p>Café — open</p>", "Café — open").as_deref(),
            Some("Café — open")
        );
    }
}
