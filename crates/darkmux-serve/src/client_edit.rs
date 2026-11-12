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
/// the source it came from. One pass over the source's chars, so it always
/// ends: an entity's own chars are skipped, never read again.
fn rendered(source: &str) -> Vec<(char, usize, usize)> {
    let mut out = Vec::new();
    let mut skip_to = 0;
    for (i, c) in source.char_indices() {
        if i < skip_to {
            continue;
        }
        let rest = &source[i..];
        let (shown, len) = match ENTITIES.iter().find(|(e, _)| rest.starts_with(e)) {
            Some((ent, ch)) => (*ch, ent.len()),
            None => (c, c.len_utf8()),
        };
        skip_to = i + len;
        push_shown(&mut out, shown, i, skip_to);
    }
    out
}

/// Adds one char the page shows, from source bytes `start..end`.
/// Whitespace (a no-break space too, as a char or an entity) joins the run
/// directly before it, so the run reads as one space.
fn push_shown(out: &mut Vec<(char, usize, usize)>, c: char, start: usize, end: usize) {
    if !(c.is_whitespace() || c == '\u{a0}') {
        out.push((c, start, end));
        return;
    }
    match out.last_mut() {
        Some((' ', _, run_end)) if *run_end == start => *run_end = end,
        _ => out.push((' ', start, end)),
    }
}

/// The byte spans in `source` whose rendered text is `wanted` (already
/// [`normalize`]d), without overlap, at most `limit` of them, as whole
/// words: "more" is not found inside "furthermore". Each place is looked at
/// once, so it always ends.
pub fn find(source: &str, wanted: &str, limit: usize) -> Vec<(usize, usize)> {
    find_where(source, wanted, limit, whole_words)
}

/// [`find`] for an image reference (an `<img>`'s `src`, #3545): only where
/// a page uses one ([`whole_reference`]), so never part of a longer URL
/// (`hero.jpg` in `/assets/hero.jpg`, `2x.png` in `logo@2x.png`,
/// `//cdn.x.com/a.jpg` in `https://cdn.x.com/a.jpg`) and never words in the
/// page's text.
pub fn find_reference(source: &str, wanted: &str, limit: usize) -> Vec<(usize, usize)> {
    find_where(source, wanted, limit, whole_reference)
}

/// One rendered char of a page, with the source bytes it came from.
type Shown = (char, usize, usize);

/// The spans where `wanted` is the rendered text and `whole` says the match
/// at `at..end` stands on its own.
fn find_where(
    source: &str,
    wanted: &str,
    limit: usize,
    whole: fn(&[Shown], &[char], usize, usize) -> bool,
) -> Vec<(usize, usize)> {
    let want: Vec<char> = wanted.chars().collect();
    let page = rendered(source);
    let mut found = Vec::new();
    if want.is_empty() || want.len() > page.len() {
        return found;
    }
    // Where the next match may start: matches never overlap.
    let mut free = 0;
    for at in 0..=page.len() - want.len() {
        if found.len() >= limit {
            break;
        }
        let end = at + want.len();
        if at >= free && page[at..end].iter().map(|(c, _, _)| *c).eq(want.iter().copied()) && whole(&page, &want, at, end) {
            found.push((page[at].1, page[end - 1].2));
            free = end;
        }
    }
    found
}

/// Whether `want`, found at `at..end`, is whole words: a letter or digit
/// on either side of one it starts or ends with cuts it.
fn whole_words(page: &[Shown], want: &[char], at: usize, end: usize) -> bool {
    let alnum = |i: Option<usize>| i.and_then(|i| page.get(i)).is_some_and(|(c, _, _)| c.is_alphanumeric());
    let cut_word = (want[0].is_alphanumeric() && alnum(at.checked_sub(1)))
        || (want[want.len() - 1].is_alphanumeric() && alnum(Some(end)));
    !cut_word
}

/// Whether the match at `at..end` is a reference as a page uses one: it
/// opens the file, follows a quote or `(` (`src="…"`, `url(…)`, Markdown's
/// `](…)`), or follows a space that follows one of those or a comma (a
/// srcset's next candidate); and it ends the file or is followed by a
/// quote, `)`, `>`, a space, `?` or `#` (a query or fragment), or `,` (a
/// srcset). Every other character (`/`, `.`, `@`, `:`, `=`, `+`, `,` before
/// it, a letter) continues a URL, so a match there is part of a longer one.
/// An unquoted attribute value (`src=hero.jpg`) is not found.
fn whole_reference(page: &[Shown], _want: &[char], at: usize, end: usize) -> bool {
    let opens = |c: char| matches!(c, '"' | '\'' | '`' | '(');
    let before = |back: usize| at.checked_sub(back).map(|i| page[i].0);
    let opened = match before(1) {
        None => true,
        Some(' ') => before(2).is_none_or(|c| opens(c) || c == ','),
        Some(c) => opens(c),
    };
    let closed = page.get(end).is_none_or(|(c, _, _)| matches!(c, '"' | '\'' | '`' | ')' | '>' | ' ' | '?' | '#' | ','));
    opened && closed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(source: &str, wanted: &str) -> Option<String> {
        let hits = find(source, &normalize(wanted), 2);
        (hits.len() == 1).then(|| source[hits[0].0..hits[0].1].to_string())
    }

    fn spans<'a>(source: &'a str, wanted: &str) -> Vec<&'a str> {
        find_reference(source, wanted, 3).into_iter().map(|(a, b)| &source[a..b]).collect::<Vec<_>>()
    }

    /// (review) A reference is a whole URL: never the tail or the head of
    /// a longer one, whatever URL characters surround it.
    #[test]
    fn a_reference_is_never_found_inside_a_longer_url() {
        assert!(spans("<img src=\"/assets/hero.jpg\">", "hero.jpg").is_empty());
        assert!(spans("<img src=\"/hero.jpg.bak\">", "/hero.jpg").is_empty());
        assert!(spans("<img src=\"my_hero.jpg\"><img src=\"x-hero.jpg\">", "hero.jpg").is_empty());
        assert!(spans("<img src=\"/img/logo@2x.png\">", "2x.png").is_empty());
        for joined in ["/a+hero.jpg", "/a~hero.jpg", "/a:hero.jpg", "/a,hero.jpg", "/a=hero.jpg", "/a%20hero.jpg", "/a!hero.jpg"] {
            assert!(spans(&format!("<img src=\"{joined}\">"), "hero.jpg").is_empty(), "{joined}");
        }
        assert!(spans("<img src=\"https://cdn.x.com/hero.jpg\">", "//cdn.x.com/hero.jpg").is_empty());
        assert!(spans("<img src=\"../hero.jpg\">", "./hero.jpg").is_empty());
        assert!(spans("const u = `${base}/hero.jpg`;", "/hero.jpg").is_empty());
        let src = "<img src=\"/hero.jpg\"><img src=\"/thumbs/hero.jpg\">";
        assert_eq!(spans(src, "/hero.jpg"), ["/hero.jpg"]);
        // A hand edit's words still end at a slash or a dot, as prose does.
        assert_eq!(one("<p>tea/coffee</p>", "coffee").as_deref(), Some("coffee"));
    }

    /// (review) Words in a page's text are not a reference the page uses.
    #[test]
    fn a_reference_is_never_found_in_a_pages_text() {
        assert!(spans("<img src=\"/hero.jpg\"> See hero.jpg, the hero.jpg.", "hero.jpg").is_empty());
        assert!(spans("<p>hero.jpg</p>\nhero.jpg is here", "hero.jpg").is_empty());
    }

    /// (review) Every place a page names an image is still found: a query
    /// or fragment after it, `url(...)`, a srcset, Markdown, unquoted ends of
    /// the file, CRLF, a Unicode name, and an entity the page decodes.
    #[test]
    fn a_reference_is_found_wherever_a_page_uses_one() {
        assert_eq!(spans("<img src=\"/hero.jpg?v=2\">", "/hero.jpg"), ["/hero.jpg"]);
        assert_eq!(spans("<img src='/hero.jpg#top'>", "/hero.jpg"), ["/hero.jpg"]);
        assert_eq!(spans(".a { background: url(/hero.jpg); }", "/hero.jpg"), ["/hero.jpg"]);
        assert_eq!(spans(".a { background: url( \"/hero.jpg\" ); }", "/hero.jpg"), ["/hero.jpg"]);
        assert_eq!(spans("<img srcset=\"/a.jpg 1x, /hero.jpg 2x\">", "/hero.jpg"), ["/hero.jpg"]);
        assert_eq!(spans("<img srcset=\"/hero.jpg 1x, /b.jpg 2x\">", "/hero.jpg"), ["/hero.jpg"]);
        assert_eq!(spans("<img srcset=\"/a.jpg 1x,\r\n  /hero.jpg 2x\">", "/hero.jpg"), ["/hero.jpg"]);
        assert_eq!(spans("![Hero](./hero.jpg \"title\")", "./hero.jpg"), ["./hero.jpg"]);
        assert_eq!(spans("<img src={\"/hero.jpg\"} />", "/hero.jpg"), ["/hero.jpg"]);
        assert_eq!(spans("/hero.jpg", "/hero.jpg"), ["/hero.jpg"], "the whole file");
        assert_eq!(spans("<img src=\"/héros/été.jpg\">", "/héros/été.jpg"), ["/héros/été.jpg"]);
        assert_eq!(spans("<img src=\"/a.jpg?x=1&amp;y=2\">", "/a.jpg?x=1&y=2"), ["/a.jpg?x=1&amp;y=2"]);
        assert_eq!(spans("<img src=\"/hero.jpg\">\r\n<img src=\"/hero.jpg\">", "/hero.jpg").len(), 2);
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
    fn edge_inputs_end_with_what_they_hold() {
        assert!(find("", "a", 2).is_empty());
        assert!(find("ab", "abc", 2).is_empty());
        assert!(find("a a a", "a", 0).is_empty());
        assert_eq!(find("a a a", "a", 9).len(), 3);
        assert_eq!(find("x &amp;", "&", 2), vec![(2, 7)]);
        // A no-break space entity and the spaces after it are one run.
        assert_eq!(one("Hi&nbsp;\n there", "Hi there").as_deref(), Some("Hi&nbsp;\n there"));
    }

    #[test]
    fn non_ascii_text_matches_on_char_boundaries() {
        assert_eq!(
            one("<p>Café — open</p>", "Café — open").as_deref(),
            Some("Café — open")
        );
    }
}
