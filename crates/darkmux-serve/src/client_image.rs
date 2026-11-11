//! An image a paired app adds to a workspace (#3545): the bytes it sent,
//! checked for what they are, and the name darkmux gives them. Pure logic;
//! the route is `client_pairing::workspace_image_handler`.

/// The largest image darkmux takes, decoded.
pub const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

/// The folder images go in, from the work copy's root. A Vite (or most any
/// static) site serves it at `/`, so `public/images/x.png` is `/images/x.png`.
pub const IMAGE_FOLDER: &str = "public/images";

/// The kinds darkmux takes, by their first bytes, never by the name the app
/// sent. SVG is not among them: it can carry script.
pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else {
        None
    }
}

/// Standard base64 (padding optional, ASCII whitespace ignored), or `None`
/// when it is not that.
pub fn decode_base64(text: &str) -> Option<Vec<u8>> {
    fn value(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a') as u32 + 26),
            b'0'..=b'9' => Some((c - b'0') as u32 + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut digits: Vec<u8> = text.bytes().filter(|c| !c.is_ascii_whitespace()).collect();
    while digits.last() == Some(&b'=') {
        digits.pop();
    }
    if digits.len() % 4 == 1 || text.bytes().filter(|c| *c == b'=').count() > 2 {
        return None;
    }
    let mut out = Vec::with_capacity(digits.len() * 3 / 4);
    for chunk in digits.chunks(4) {
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            n |= value(*c)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

/// The file's name: a slug of the name the app sent (its stem, lowercase
/// ASCII letters and digits joined by `-`, at most 40 characters, `image`
/// when nothing is left), a short hash of the bytes, and the sniffed kind.
/// Never a folder and never a dot file, whatever was sent.
pub fn file_name(sent: &str, bytes: &[u8], kind: &str) -> String {
    let stem = sent.rsplit(['/', '\\']).next().unwrap_or("");
    let stem = stem.rsplit_once('.').map_or(stem, |(s, _)| s);
    let mut slug = String::new();
    for c in stem.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
        if slug.len() >= 40 {
            break;
        }
    }
    let slug = slug.trim_end_matches('-');
    let slug = if slug.is_empty() { "image" } else { slug };
    format!("{slug}-{:08x}.{kind}", fnv1a(bytes) as u32)
}

/// FNV-1a, 64-bit: tells two images apart in a name, not a security hash.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ *b as u64).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_the_four_kinds_and_refuses_svg_and_text() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\nrest"), Some("png"));
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("jpg"));
        assert_eq!(sniff(b"GIF89a.."), Some("gif"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some("webp"));
        assert_eq!(
            sniff(b"<svg xmlns='http://www.w3.org/2000/svg'><script/></svg>"),
            None
        );
        assert_eq!(sniff(b"hello"), None);
        assert_eq!(sniff(b""), None);
    }

    #[test]
    fn decodes_base64_with_and_without_padding() {
        assert_eq!(decode_base64("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(decode_base64("aGVsbG8").unwrap(), b"hello");
        assert_eq!(decode_base64("aGVs\nbG8h").unwrap(), b"hello!");
        assert_eq!(decode_base64("").unwrap(), b"");
        assert!(decode_base64("a").is_none());
        assert!(decode_base64("aGV$bG8=").is_none());
        assert!(decode_base64("aG===").is_none());
    }

    #[test]
    fn a_name_is_a_slug_never_a_path_or_dot_file() {
        let png = b"\x89PNG\r\n\x1a\n";
        let n = file_name("My Logo (final).PNG", png, "png");
        assert!(
            n.starts_with("my-logo-final-") && n.ends_with(".png"),
            "{n}"
        );
        for sent in ["../../etc/passwd", ".hidden", "", "///", "a/b\\..\\c.jpg"] {
            let n = file_name(sent, png, "png");
            assert!(
                !n.contains('/') && !n.contains('\\') && !n.starts_with('.') && !n.contains(".."),
                "{sent:?} -> {n}"
            );
        }
        assert!(file_name("", png, "png").starts_with("image-"));
        assert!(file_name(&"x".repeat(200), png, "png").len() <= 40 + 1 + 8 + 4);
        assert_ne!(
            file_name("a", b"one", "png"),
            file_name("a", b"two", "png"),
            "the bytes tell them apart"
        );
    }
}
