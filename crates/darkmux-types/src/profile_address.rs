//! (#2916 stage 2) Profile addresses: `profile@machine`.
//!
//! A profile reference may name the machine that owns the profile
//! (`host@studio`). The owning machine resolves the profile against its own
//! registry and loads it; every other machine only refers to it. One parser,
//! here, so every consumer reads an address the same way (contract 1).
//!
//! The address is split at the LAST `@`. The machine part is a machine name
//! (`machine_id`): `[A-Za-z0-9_-]`, 1..=64, compared case-insensitively. A
//! profile part that itself contains `@` is refused: a profile name never
//! contains `@` (#2916 decision 9), so an address always reads one way.
//!
//! Storage stays a plain string (contract 7): a config value or a flag is
//! parsed where it is consumed, and a malformed one is refused there.

/// The longest machine name (matches the fleet wire's identifier cap).
pub const MAX_MACHINE_NAME_LEN: usize = 64;

/// A parsed profile reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileAddress {
    /// The profile name, as the owning machine knows it.
    pub profile: String,
    /// The owning machine, when the reference names one.
    pub machine: Option<String>,
}

impl ProfileAddress {
    /// Parse a profile reference. A reference with no `@` is a plain profile
    /// name (returned as written, unvalidated: its meaning is the local
    /// registry's). A reference with `@` must be `<profile>@<machine>` with
    /// both parts well-formed; anything else is an error naming the fix.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let Some((profile, machine)) = raw.rsplit_once('@') else {
            return Ok(Self { profile: raw.to_string(), machine: None });
        };
        if profile.is_empty() {
            return Err(format!(
                "profile address `{raw}` names no profile: write `<profile>@<machine>`, e.g. `host@studio`"
            ));
        }
        if profile.contains('@') {
            return Err(format!(
                "profile address `{raw}` has more than one `@`: a profile name never contains `@`, so \
                 write exactly `<profile>@<machine>`"
            ));
        }
        if let Some(problem) = machine_name_problem(machine) {
            return Err(format!("profile address `{raw}`: {problem}"));
        }
        Ok(Self { profile: profile.to_string(), machine: Some(machine.to_string()) })
    }

    /// True when `raw` is written as an address (contains `@`), whether or
    /// not it is well-formed.
    pub fn is_address(raw: &str) -> bool {
        raw.contains('@')
    }
}

impl std::fmt::Display for ProfileAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.machine {
            Some(m) => write!(f, "{}@{m}", self.profile),
            None => f.write_str(&self.profile),
        }
    }
}

/// Why `value` is not a machine name, or `None` when it is one:
/// `[A-Za-z0-9_-]`, 1..=[`MAX_MACHINE_NAME_LEN`]. The fleet wire adds one
/// more rule of its own (no `-from-`), checked where a job is built.
pub fn machine_name_problem(value: &str) -> Option<String> {
    if value.is_empty() {
        return Some("names no machine: write `<profile>@<machine>`, e.g. `host@studio`".into());
    }
    if value.len() > MAX_MACHINE_NAME_LEN {
        return Some(format!("the machine name is longer than {MAX_MACHINE_NAME_LEN} characters"));
    }
    if let Some(c) = value.chars().find(|c| !(c.is_ascii_alphanumeric() || *c == '-' || *c == '_')) {
        return Some(format!(
            "the machine name `{value}` contains {c:?}; a machine name (its `machine_id`) is letters, \
             digits, `-` and `_`"
        ));
    }
    None
}

/// The refusal for a profile ADDRESS reaching a path that only runs on this
/// machine, or `None` when `profile` is a plain name. Such a path must never
/// read `host@studio` as an undefined local name (which would fall to
/// `default_profile` and run something the caller did not ask for, here).
pub fn local_only_refusal(profile: &str, path: &str) -> Option<String> {
    if !ProfileAddress::is_address(profile) {
        return None;
    }
    Some(format!(
        "profile `{profile}` is an address (`<profile>@<machine>`), and {path} runs only on this \
         machine; it never runs an addressed profile here. Name a profile on this machine, or send \
         the work with `darkmux dispatch <role> --profile {profile}`"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(raw: &str) -> ProfileAddress {
        ProfileAddress::parse(raw).unwrap_or_else(|e| panic!("{raw}: {e}"))
    }

    #[test]
    fn a_plain_name_is_a_local_reference() {
        assert_eq!(ok("host"), ProfileAddress { profile: "host".into(), machine: None });
        // Plain names are the local registry's to judge, not the parser's.
        assert_eq!(ok("My.Profile").profile, "My.Profile");
    }

    #[test]
    fn an_address_splits_into_profile_and_machine() {
        let a = ok("host@studio");
        assert_eq!(a.profile, "host");
        assert_eq!(a.machine.as_deref(), Some("studio"));
        assert_eq!(a.to_string(), "host@studio");
        // Capitals are allowed: a hostname-derived machine_id has them.
        assert_eq!(ok("coder@MacBook-Pro").machine.as_deref(), Some("MacBook-Pro"));
        assert_eq!(ok("a_b@m_1").profile, "a_b");
    }

    #[test]
    fn a_malformed_address_is_refused_with_the_shape_named() {
        for (raw, needle) in [
            ("@studio", "names no profile"),
            ("host@", "names no machine"),
            ("a@b@c", "more than one `@`"),
            ("host@stu dio", "contains ' '"),
            ("host@studio.tail", "contains '.'"),
            ("host@stu/dio", "contains '/'"),
        ] {
            let err = ProfileAddress::parse(raw).expect_err(raw);
            assert!(err.contains(needle), "{raw}: {err}");
            assert!(err.contains(raw), "names the raw value: {err}");
        }
        let long = format!("host@{}", "m".repeat(MAX_MACHINE_NAME_LEN + 1));
        assert!(ProfileAddress::parse(&long).unwrap_err().contains("longer than"));
        let edge = format!("host@{}", "m".repeat(MAX_MACHINE_NAME_LEN));
        assert!(ProfileAddress::parse(&edge).is_ok());
    }

    #[test]
    fn a_local_only_path_refuses_an_address_and_passes_a_plain_name() {
        assert_eq!(local_only_refusal("host", "the lab"), None);
        let msg = local_only_refusal("host@studio", "the lab").unwrap();
        assert!(msg.contains("host@studio") && msg.contains("the lab") && msg.contains("only on this machine"), "{msg}");
        // A malformed address is still an address: never read as a local name.
        assert!(local_only_refusal("@", "x").is_some());
    }
}
