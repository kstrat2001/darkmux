//! Shell quoting for commands darkmux prints for an operator to paste.

/// Shell-quote one argument for a command an operator is expected to PASTE.
/// A path with a space in it (`~/My Projects/...`) otherwise produces a
/// command that silently means something else. Plain words pass through
/// unquoted so the common case stays readable.
pub fn quote(s: &str) -> String {
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-./:@+=,".contains(&b)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::quote;

    #[test]
    fn plain_words_pass_through_and_everything_else_is_single_quoted() {
        assert_eq!(quote("/Users/me/.darkmux/lab"), "/Users/me/.darkmux/lab");
        assert_eq!(quote("/a b/lab"), "'/a b/lab'");
        assert_eq!(quote(""), "''");
        assert_eq!(quote("it's"), r"'it'\''s'");
    }
}
