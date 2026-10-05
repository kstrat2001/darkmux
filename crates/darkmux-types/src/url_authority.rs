//! (#3074) The ONE URL authority parser. A credential-bearing URL (a Redis
//! hub URL, `redis://user:pw@host:6379/0`) is read by the connection parser
//! (the `redis`/`url` crates), by the flow redactor (which masks the
//! password) and by the serve redactor (which hides the host). Three readers
//! that split the URL three ways leak exactly where they disagree: a password
//! holding `#`, `/` or `?` made one reader find the host at the password's
//! first delimiter. Both redactors split through [`UrlAuthority::parse`], so
//! they cannot disagree.
//!
//! The rule is fail-closed. A redis URL has no path `@` (the path is the
//! database number), so an `@` after the password's own delimiter can only be
//! the userinfo/host boundary: the userinfo is everything between `://` and
//! the LAST `@`. The cost is a misread host for a URL carrying a stray `@`
//! after its host, in a diagnostic; the alternative is a leaked password.

/// A URL split at `://` and at the last `@`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UrlAuthority<'a> {
    /// What precedes `://`.
    pub scheme: &'a str,
    /// Everything between `://` and the last `@`, or `None` with no `@`.
    pub userinfo: Option<&'a str>,
    /// Everything after the boundary: `host:port/path?query#fragment`.
    pub host_and_tail: &'a str,
}

impl<'a> UrlAuthority<'a> {
    /// `None` when `url` has no `scheme://`.
    pub fn parse(url: &'a str) -> Option<Self> {
        let (scheme, rest) = url.split_once("://")?;
        let (userinfo, host_and_tail) = match rest.rsplit_once('@') {
            Some((u, h)) => (Some(u), h),
            None => (None, rest),
        };
        Some(Self { scheme, userinfo, host_and_tail })
    }

    /// `host:port`, ending at the first `/`, `?` or `#` of the tail. Empty
    /// for a unix-socket URL (`redis+unix:///tmp/x.sock`).
    pub fn hostport(&self) -> &'a str {
        let end = self.host_and_tail.find(['/', '?', '#']).unwrap_or(self.host_and_tail.len());
        &self.host_and_tail[..end]
    }

    /// The URL's `?query` (without the `?`, up to any `#`), when it has one.
    pub fn query(&self) -> Option<&'a str> {
        let (_, after) = self.host_and_tail.split_once('?')?;
        Some(after.split('#').next().unwrap_or(after))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_at_the_last_at_whatever_the_password_holds() {
        for pw in ["p#ss", "p/ss", "p?ss", "a#b/c?d", "p@ss"] {
            let url = format!("redis://kain:{pw}@real.host:6379/0");
            let a = UrlAuthority::parse(&url).unwrap();
            assert_eq!(a.userinfo, Some(format!("kain:{pw}").as_str()), "{pw}");
            assert_eq!(a.hostport(), "real.host:6379", "{pw}");
        }
    }

    #[test]
    fn no_userinfo_and_unix_socket_shapes() {
        let a = UrlAuthority::parse("redis://h:6379/2").unwrap();
        assert_eq!((a.userinfo, a.hostport()), (None, "h:6379"));
        let u = UrlAuthority::parse("redis+unix:///tmp/x.sock?pass=s3&db=1#f").unwrap();
        assert_eq!((u.scheme, u.hostport(), u.query()), ("redis+unix", "", Some("pass=s3&db=1")));
        assert_eq!(UrlAuthority::parse("garbage"), None);
    }
}
