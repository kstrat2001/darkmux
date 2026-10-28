//! (#3397) The built-in provider registry: the hosted model APIs darkmux
//! knows by their host.
//!
//! One table, in code and nowhere else. It ships inside a tagged release, so
//! it is covered by the release and commit checks, and it cannot be edited
//! locally: there is no config key, env var or file that adds or changes a
//! row. Two consumers read it, and nothing else decides either question:
//!
//! - **The request dialect** an endpoint speaks when it declares none
//!   ([`crate::ModelEndpoint::resolved_dialect`], #3173): Anthropic's native
//!   Messages API, the only way its prompt caching applies.
//! - **Where a lab run was served** (`lab submit`, #3397): a run on a listed
//!   host has hardware `unknown` (the provider's machines, not the one that
//!   submits it), and the bundle's `engine` is the provider's name.
//!
//! A host is matched label by label, case-insensitively, with any port and
//! one trailing dot dropped. `*` in a pattern stands for exactly one DNS
//! label, so `*.openai.azure.com` takes `myres.openai.azure.com` and never
//! `evil-openai.azure.com.attacker.net` or `a.b.openai.azure.com`.

use crate::endpoint::Dialect;

/// One hosted model API darkmux knows by its host.
#[derive(Debug, PartialEq, Eq)]
pub struct Provider {
    /// The provider's name, as `lab submit` sends it in a bundle's `engine`
    /// (the site's stack-string form: letters, digits and single spaces).
    pub name: &'static str,
    /// The API hosts, as DNS labels; `*` stands for exactly one label.
    hosts: &'static [&'static str],
    /// The dialect an endpoint on this provider speaks when it declares
    /// none; `None` keeps the unmanaged default.
    pub default_dialect: Option<Dialect>,
}

/// THE registry. A row is a provider's real API host, never a guess at one.
pub static PROVIDERS: &[Provider] = &[
    Provider { name: "Anthropic API", hosts: &["api.anthropic.com"], default_dialect: Some(Dialect::Messages) },
    Provider { name: "OpenAI API", hosts: &["api.openai.com"], default_dialect: None },
    Provider { name: "Azure OpenAI", hosts: &["*.openai.azure.com"], default_dialect: None },
    Provider { name: "Google Gemini API", hosts: &["generativelanguage.googleapis.com"], default_dialect: None },
    Provider {
        name: "Amazon Bedrock",
        hosts: &["bedrock-runtime.*.amazonaws.com", "bedrock-runtime-fips.*.amazonaws.com"],
        default_dialect: None,
    },
    Provider { name: "OpenRouter", hosts: &["openrouter.ai"], default_dialect: None },
];

/// The provider whose API is at `hostport` (an authority without userinfo,
/// as [`crate::endpoint::url_host`] gives it: a name, an optional port).
/// `None` for any host the registry does not list.
pub fn provider_of_host(hostport: &str) -> Option<&'static Provider> {
    let host = crate::panel_audience::host_of(hostport);
    let host = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
    PROVIDERS.iter().find(|p| p.hosts.iter().any(|pattern| host_matches(pattern, &host)))
}

/// `host` (lowercase, no port, no trailing dot) against one pattern, label
/// by label. A wildcard label is one non-empty run of letters, digits and
/// hyphens: never a dot, so it never spans two labels.
fn host_matches(pattern: &str, host: &str) -> bool {
    let (want, got): (Vec<&str>, Vec<&str>) = (pattern.split('.').collect(), host.split('.').collect());
    want.len() == got.len()
        && want.iter().zip(&got).all(|(w, g)| match *w {
            "*" => !g.is_empty() && g.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            label => label == *g,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(hostport: &str) -> Option<&'static str> {
        provider_of_host(hostport).map(|p| p.name)
    }

    /// Each provider by its real API host, in any case, with a port or a
    /// trailing dot.
    #[test]
    fn each_provider_is_found_by_its_api_host_however_it_is_spelled() {
        for (host, want) in [
            ("api.anthropic.com", "Anthropic API"),
            ("API.Anthropic.COM", "Anthropic API"),
            ("api.anthropic.com:443", "Anthropic API"),
            ("api.anthropic.com.", "Anthropic API"),
            ("api.anthropic.com.:8443", "Anthropic API"),
            ("api.openai.com", "OpenAI API"),
            ("my-resource.openai.azure.com", "Azure OpenAI"),
            ("My-Resource.OpenAI.Azure.com:443", "Azure OpenAI"),
            ("generativelanguage.googleapis.com", "Google Gemini API"),
            ("bedrock-runtime.us-east-1.amazonaws.com", "Amazon Bedrock"),
            ("bedrock-runtime-fips.us-gov-west-1.amazonaws.com", "Amazon Bedrock"),
            ("openrouter.ai", "OpenRouter"),
        ] {
            assert_eq!(name(host), Some(want), "{host}");
        }
    }

    /// A lookalike is not the provider: a listed host as a prefix or a
    /// suffix of another, a label the wildcard would have to span, an empty
    /// label, or a host that only resembles one.
    #[test]
    fn a_lookalike_host_is_no_provider() {
        for host in [
            "evil-openai.azure.com.attacker.net",
            "api.openai.com.evil.net",
            "api.anthropic.com.evil.example",
            "evilapi.anthropic.com",
            "xapi.openai.com",
            "openai.azure.com",
            ".openai.azure.com",
            "a.b.openai.azure.com",
            "my_res.openai.azure.com",
            "bedrock-runtime.amazonaws.com",
            "bedrock-runtime.us-east-1.eu.amazonaws.com",
            "bedrock.us-east-1.amazonaws.com",
            "openrouter.ai.attacker.net",
            "api.openrouter.ai",
            "api.anthropic.com..",
            "localhost:1234",
            "192.168.1.20:8080",
            "[::1]:8080",
            "",
        ] {
            assert_eq!(name(host), None, "{host}");
        }
    }

    /// Every name fits the site's stack-string form (`STACK_TEXT` in
    /// darklyenergized's bundle.ts), which a bundle's `engine` is held to.
    #[test]
    fn every_provider_name_is_a_stack_string() {
        for p in PROVIDERS {
            let n = p.name;
            let allowed = |c: char| c.is_ascii_alphanumeric() || "._:/@()+,-".contains(c);
            assert!(n.starts_with(|c: char| c.is_ascii_alphanumeric()) && n.len() <= 80, "{n}");
            assert!(n.chars().all(|c| allowed(c) || c == ' ') && !n.contains("  ") && !n.ends_with(' '), "{n}");
        }
    }
}
