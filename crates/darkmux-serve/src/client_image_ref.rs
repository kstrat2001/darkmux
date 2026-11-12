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
//! Never words in a page's text, a Markdown code span, alt text, or a
//! repository's own documents ([`outside_the_site`]: a README, a changelog,
//! anything under `.github/`), which are not the site's pages. A query or
//! fragment after the reference (`/x.jpg?v=2`) stays as it was.

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

/// The spans in `source` (the tracked file `file`) where the reference
/// `wanted` (already normalized) is used, at most `limit` of them.
pub fn find_reference(file: &str, source: &str, wanted: &str, limit: usize) -> Vec<(usize, usize)> {
    match kind_of(file) {
        Some(kind) => crate::client_edit::spans_of(source, wanted, limit, |s, e| used_at(kind, source, s, e)),
        None => Vec::new(),
    }
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
/// changelog, contributing guide, license, code of conduct or security
/// policy at any depth, and anything under `.github/`. A `docs/` folder is
/// not among them: it is often a docs site.
fn outside_the_site(file: &str) -> bool {
    let name = file.rsplit('/').next().unwrap_or(file).to_ascii_uppercase();
    let stem = name.split('.').next().unwrap_or("");
    file.starts_with(".github/")
        || matches!(stem, "README" | "CHANGELOG" | "CHANGES" | "CONTRIBUTING" | "LICENSE" | "LICENCE" | "CODE_OF_CONDUCT" | "SECURITY")
}

/// Whether the match at `s..e` is a reference `kind`'s file uses.
fn used_at(kind: Kind, src: &str, s: usize, e: usize) -> bool {
    let e = past_query(src, e);
    in_css_url(src, s, e) || used_as(kind, src, s, e)
}

/// Whether `kind`'s file uses a URL at `s..e` its own way (`url(...)` aside).
fn used_as(kind: Kind, src: &str, s: usize, e: usize) -> bool {
    match kind {
        Kind::Markup => in_url_attribute(src, s, e) || (in_script(src, s) && in_code_literal(src, s, e)),
        Kind::Code => in_code_literal(src, s, e),
        Kind::Markdown => in_markdown(src, s, e),
        Kind::Yaml => in_yaml_value(src, s, e),
        Kind::Toml => in_toml_value(src, s, e),
        Kind::Json => in_quoted_value(src, s, e, &["\""]),
        Kind::Style => false,
    }
}

/// Where a Markdown file uses a URL: an image or link target, a reference
/// definition, a front matter value, an HTML attribute, an MDX import.
fn in_markdown(src: &str, s: usize, e: usize) -> bool {
    in_markdown_target(src, s, e)
        || in_reference_definition(src, s, e)
        || in_front_matter(src, s, e)
        || in_url_attribute(src, s, e)
        || (src[line_start(src, s)..].starts_with("import ") && in_code_literal(src, s, e))
}

/// `e` moved past a query or fragment that follows the reference (`?v=2`,
/// `#top`), to where the URL ends.
fn past_query(src: &str, e: usize) -> usize {
    if !src[e..].starts_with(['?', '#']) {
        return e;
    }
    let rest = &src[e..];
    let end = rest
        .char_indices()
        .find(|(i, c)| c.is_whitespace() || matches!(c, '"' | '\'' | '`' | ')' | '<' | '>' | ',') || rest[*i..].starts_with("&quot;"))
        .map_or(rest.len(), |(i, _)| i);
    e + end
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
/// (an entity too) or not, spaces allowed.
fn in_css_url(src: &str, s: usize, e: usize) -> bool {
    let (open, close) = match quoted(src, s, e, QUOTES) {
        Some((q, at)) => (at, e + q.len()),
        None => (s, e),
    };
    let left = src[..open].trim_end();
    let opened = left.len() >= 4 && left[left.len() - 4..].eq_ignore_ascii_case("url(");
    opened && src[close..].trim_start().starts_with(')')
}

/// The name of the attribute (or key) whose `=` ends at `at`, spaces
/// allowed around it: `src` in `src = "`.
fn attribute_before(src: &str, at: usize) -> Option<&str> {
    let left = src[..at].trim_end().strip_suffix('=')?.trim_end();
    let start = left
        .rfind(|c: char| !(c.is_alphanumeric() || matches!(c, '-' | '_' | ':' | '@' | '.')))
        .map_or(0, |i| i + c_len(left, i));
    let name = &left[start..];
    (!name.is_empty()).then_some(name)
}

/// The byte length of the char at `i` in `text`.
fn c_len(text: &str, i: usize) -> usize {
    text[i..].chars().next().map_or(1, char::len_utf8)
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
fn in_url_attribute(src: &str, s: usize, e: usize) -> bool {
    in_srcset(src, s, e)
        || match quoted(src, s, e, QUOTES) {
            Some((q, at)) => attribute_value_at(src, at, e + q.len()).is_some_and(is_url_attribute),
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
fn in_srcset(src: &str, s: usize, e: usize) -> bool {
    let Some(q) = src[..s].rfind(['"', '\'']) else {
        return false;
    };
    let quote = src[q..].chars().next().unwrap_or('"');
    let before = src[q + 1..s].trim_end();
    let first_or_next = before.is_empty() || before.ends_with(',');
    let ended = src[e..].starts_with(|c: char| c.is_whitespace() || c == ',' || c == quote);
    let name = attribute_before(src, q).map(bare_attribute);
    first_or_next && ended && name.is_some_and(|n| SRCSET_ATTRIBUTES.contains(&n.as_str()))
}

/// Whether `s` is inside a `<script>` element, or an Astro component's
/// leading `---` fence.
fn in_script(src: &str, s: usize) -> bool {
    let before = &src[..s];
    let opened = before.rfind("<script");
    let opened = opened.is_some() && opened > before.rfind("</script");
    opened || (before.starts_with("---") && !before[3..].contains("\n---"))
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
    let key = match left.strip_suffix(':') {
        Some(k) => k.trim_end().trim_end_matches(['"', '\'']),
        None => return attribute_before(src, left.len()).is_some_and(is_text_key),
    };
    let start = key.rfind(|c: char| !(c.is_alphanumeric() || matches!(c, '-' | '_'))).map_or(0, |i| i + c_len(key, i));
    is_text_key(&key[start..])
}

fn is_text_key(name: &str) -> bool {
    TEXT_KEYS.contains(&bare_attribute(name).as_str())
}

/// Where the line holding byte `at` starts.
fn line_start(src: &str, at: usize) -> usize {
    src[..at].rfind('\n').map_or(0, |i| i + 1)
}

/// The rest of the line from `at`, without its line break.
fn rest_of_line(src: &str, at: usize) -> &str {
    let rest = &src[at..];
    rest[..rest.find('\n').unwrap_or(rest.len())].trim_end_matches('\r')
}

/// Whether what follows `at` on its line is only spaces, or a ` #` comment.
fn ends_line(src: &str, at: usize) -> bool {
    let rest = rest_of_line(src, at).trim_start();
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
fn in_reference_definition(src: &str, s: usize, e: usize) -> bool {
    let (open, close) = angled(src, s, e);
    let head = &src[line_start(src, open)..open];
    let head = head.trim_start_matches(' ');
    let defined = head.starts_with('[')
        && head.trim_end().strip_suffix(':').is_some_and(|h| h.ends_with(']'))
        && head.ends_with([' ', '\t']);
    let rest = rest_of_line(src, close);
    defined && (rest.is_empty() || rest.starts_with([' ', '\t']))
}

/// A value in a Markdown file's leading front matter: YAML between `---`
/// lines, TOML between `+++` lines.
fn in_front_matter(src: &str, s: usize, e: usize) -> bool {
    for (fence, value) in [("---", in_yaml_value as fn(&str, usize, usize) -> bool), ("+++", in_toml_value)] {
        let opened = src.starts_with(fence) && src[fence.len()..].starts_with(['\n', '\r']);
        let closed_after = src[..s].find(&format!("\n{fence}")).is_none_or(|i| i == 0);
        if opened && closed_after && src[s..].contains(&format!("\n{fence}")) {
            return value(src, s, e);
        }
    }
    false
}

/// A YAML value: `key: x` or a list item `- x`, quoted or not, alone on its
/// line (a ` #` comment after it allowed), unless its key is a text key.
fn in_yaml_value(src: &str, s: usize, e: usize) -> bool {
    let (open, close) = match quoted(src, s, e, &["\"", "'"]) {
        Some((q, at)) => (at, e + q.len()),
        None => (s, e),
    };
    let head = src[line_start(src, open)..open].trim_start();
    let keyed = head.strip_suffix(' ').map(str::trim_end).and_then(|h| h.strip_suffix(':'));
    let valued = match keyed {
        Some(key) => !key.is_empty() && !is_text_key(key.trim_matches(['"', '\''])),
        None => head == "- ",
    };
    valued && ends_line(src, close)
}

/// A TOML value: `key = "x"` or `key = 'x'`, alone on its line, unless its
/// key is a text key.
fn in_toml_value(src: &str, s: usize, e: usize) -> bool {
    let Some((q, at)) = quoted(src, s, e, &["\"", "'"]) else {
        return false;
    };
    let head = src[line_start(src, at)..at].trim();
    let key = head.strip_suffix('=').map(str::trim_end);
    key.is_some_and(|k| !k.is_empty() && !is_text_key(k)) && ends_line(src, e + q.len())
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
            ("README.md", "![logo](/hero.jpg)\n", "/hero.jpg"),
            ("docs/CHANGELOG.md", "![logo](/hero.jpg)\n", "/hero.jpg"),
            ("notes.txt", "src=\"/hero.jpg\"", "/hero.jpg"),
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
