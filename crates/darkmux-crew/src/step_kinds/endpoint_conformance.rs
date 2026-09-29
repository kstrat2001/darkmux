//! (#2902 step 3) Endpoint conformance: one resolver, one chat-URL builder,
//! one host extraction, one credential order, one body builder.
//!
//! Before #2902 each of these was written two or three times (three
//! profile→model resolvers, three "is this remote?" tests, two chat-URL
//! builders, three hosted body builders, three host extractions, the
//! credential order twice), and a fix to one copy left the others behind
//! (#2904, #2905). They now live in `darkmux_types::endpoint` (the endpoint
//! rules) and `crate::target` (the resolver), and every consumer calls them.
//!
//! [`no_call_site_decides_an_endpoint_on_its_own`] walks every production
//! source file of the workspace (`crates/*/src` and the binary's `src`, test
//! modules and `*_tests.rs` cut out, comment lines dropped) and counts each
//! [`FORBIDDEN`] idiom per file. A count that is not exactly the one
//! [`ALLOWED`] names fails, with the instruction: route the new site through
//! the one function, or, if it genuinely is a new home for the rule, add it
//! here with the reason.
//!
//! What this sweep CANNOT see, stated so a green run is not read as more
//! than it is: it matches spellings, per line of production text. A chat URL
//! assembled from pieces split across arguments (`format!("{}/{}", base,
//! "chat")`), a classification written as a pattern match
//! (`matches!(&ep.url, Some(_))`, `if let Some(u) = &ep.url`), or a body
//! built through a renamed helper are not caught. It is a tripwire for the
//! idioms each duplicated rule was actually written in, not a proof that no
//! other spelling exists; review still owns the rest.
//!
//! Out of scope, by construction: the `runtime/` crate is not a workspace
//! member and cannot depend on darkmux-types. It receives the chat URL
//! (`--base-url`/`--chat-url`) and the dialect (`--dialect`) from the host,
//! which computes both here; its own `{base}/chat/completions` append and
//! cap-field rename act on those values.

use super::liveness_conformance::block_from;

/// An idiom that decides something about an endpoint by itself.
struct Idiom {
    pattern: &'static str,
    /// What to call instead.
    route: &'static str,
}

const FORBIDDEN: &[Idiom] = &[
    Idiom { pattern: "chat/completions", route: "ModelEndpoint::chat_url (or endpoint::lmstudio_chat_url for an explicit LM Studio base)" },
    Idiom { pattern: ".endpoint.is_some()", route: "ProfileModel::is_managed / ModelEndpoint::kind" },
    Idiom { pattern: ".endpoint.is_none()", route: "ProfileModel::is_managed / ModelEndpoint::kind" },
    Idiom { pattern: "strip_prefix(\"https://\")", route: "ModelEndpoint::host / endpoint::url_host" },
    Idiom { pattern: "strip_prefix(\"http://\")", route: "ModelEndpoint::host / endpoint::url_host" },
    Idiom { pattern: "\"max_tokens\":", route: "single_shot::chat_body" },
    Idiom { pattern: ".keychain", route: "EndpointAuth::credential_source" },
    Idiom { pattern: "default_model_id()", route: "target::resolve_in (the SELECTED model), or dispatch::profile_default_window for a role-less window" },
    Idiom { pattern: ".is_remote()", route: "ProfileModel::is_managed / ModelEndpoint::kind (managed replaces `url.is_none()`)" },
    Idiom { pattern: "get(\"endpoint\").is_some()", route: "target::step_unmanaged_endpoint" },
    Idiom { pattern: "get(\"endpoint\").is_none()", route: "target::step_unmanaged_endpoint" },
    Idiom { pattern: ".url.is_some()", route: "ModelEndpoint::kind" },
    Idiom { pattern: ".url.is_none()", route: "ModelEndpoint::kind" },
    Idiom { pattern: "split(\"://\")", route: "ModelEndpoint::host / endpoint::url_host" },
    Idiom { pattern: ".key_env", route: "EndpointAuth::credential_source" },
    Idiom { pattern: "insert(\"max_completion_tokens\"", route: "single_shot::chat_body" },
    Idiom { pattern: "insert(\"max_tokens\"", route: "single_shot::chat_body" },
    Idiom { pattern: "\"max_completion_tokens\":", route: "single_shot::chat_body" },
    Idiom { pattern: "select_model(", route: "target::resolve_in / target::select_in_profile" },
];

/// The homes: `(workspace-relative file, pattern, count, why)`.
const ALLOWED: &[(&str, &str, usize, &str)] = &[
    ("crates/darkmux-types/src/endpoint.rs", "chat/completions", 3, "THE chat-URL builder: the unmanaged form with and without api-version, and the LM Studio form"),
    ("crates/darkmux-types/src/endpoint.rs", ".url.is_some()", 1, "THE classification (kind): an explicit managed endpoint that also declares a url is refused"),
    ("crates/darkmux-types/src/endpoint.rs", ".key_env", 3, "THE credential order (credential_source), and validate()'s source check with its message"),
    ("crates/darkmux-crew/src/dispatch_internal.rs", ".key_env", 2, "resolve_endpoint_secret's Keychain-read hint naming the variable to export (message text), downstream of credential_source"),
    ("crates/darkmux-doctor/src/lib.rs", ".key_env", 1, "a message naming the field `endpoint.auth.key_env`"),
    ("crates/darkmux-crew/src/single_shot.rs", "insert(\"max_completion_tokens\"", 1, "THE body builder, chat-completions dialect"),
    ("crates/darkmux-crew/src/single_shot.rs", "insert(\"max_tokens\"", 1, "THE body builder, chat-completions-max-tokens dialect"),
    ("crates/darkmux-crew/src/target.rs", "select_model(", 1, "THE resolver's selection (select_in_profile)"),
    ("crates/darkmux-types/src/endpoint.rs", ".keychain", 3, "THE credential order (credential_source), and validate()'s source check with its message"),
    ("crates/darkmux-crew/src/dispatch_internal.rs", ".keychain", 2, "resolve_endpoint_secret: the env var vanished between credential_source and the read, fall to the same declared item; and a message naming the field"),
    ("crates/darkmux-doctor/src/lib.rs", ".keychain", 3, "the probe's dedup key (two credentials to one deployment both probe) and two messages naming the field"),
    ("crates/darkmux-flow/src/hooks.rs", "strip_prefix(\"http://\")", 2, "the hooks destination URL policy, not a model endpoint"),
    ("crates/darkmux-serve/src/runs.rs", ".endpoint.is_some()", 1, "a run aggregate's recorded `endpoint` label (from flow records), not a profile endpoint"),
    ("crates/darkmux-serve/src/runs.rs", ".endpoint.is_none()", 1, "a run aggregate's recorded `endpoint` label (from flow records), not a profile endpoint"),
    ("crates/darkmux-crew/src/dispatch.rs", "default_model_id()", 1, "profile_default_window: the role-less window, documented as never a dispatch's own"),
    ("crates/darkmux-crew/src/select.rs", "default_model_id()", 1, "select_model's own tie-break default (the selection itself)"),
    ("crates/darkmux-doctor/src/lib.rs", "default_model_id()", 2, "which profile the LOADED residents match (profile match / active profile), not a model choice"),
    ("crates/darkmux-lab/src/lab/profile_check.rs", "default_model_id()", 1, "a lab envelope warning about whether the profile's default model is loaded, not a model choice"),
    ("src/main.rs", "default_model_id()", 1, "`profile list`'s `default` marker (display)"),
    ("src/mission_config_cli.rs", "default_model_id()", 1, "`show`'s fallback for a role this binary does not know (it cannot select without the role)"),
];

/// Files that name the idioms as DATA.
const SELF: &[&str] = &["crates/darkmux-crew/src/step_kinds/endpoint_conformance.rs"];

fn workspace_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

/// Every production `.rs` file: `crates/*/src/**` and `src/**`, minus
/// `*_tests.rs` files, as workspace-relative paths.
fn production_files(root: &std::path::Path) -> Vec<String> {
    fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, root, out);
            } else if path.extension().is_some_and(|e| e == "rs")
                && !path.file_name().unwrap().to_string_lossy().ends_with("_tests.rs")
            {
                out.push(path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut out = Vec::new();
    walk(&root.join("src"), root, &mut out);
    for krate in std::fs::read_dir(root.join("crates")).unwrap() {
        walk(&krate.unwrap().path().join("src"), root, &mut out);
    }
    out.sort();
    out
}

/// `src` with every `#[cfg(test)] mod … { … }` block cut out (brace-matched
/// by the liveness lexer, so strings and comments cannot end it early) and
/// comment lines dropped.
fn production_text(src: &str) -> String {
    let mut prod = src.to_string();
    while let Some(at) = prod.find("#[cfg(test)]") {
        let after = &prod[at + "#[cfg(test)]".len()..];
        let rest = after.trim_start();
        if rest.starts_with("mod ") && rest.split('\n').next().is_some_and(|l| l.trim_end().ends_with('{')) {
            let block = block_from(rest, "mod ");
            let block_at = at + "#[cfg(test)]".len() + (after.len() - rest.len());
            let end = block_at + rest.find(&block).expect("block_from returns a slice of its input") + block.len();
            prod.replace_range(at..end, "");
        } else {
            // A `#[cfg(test)]` on an item other than a module: keep the item
            // (it is test-only, but small) and step past the attribute.
            prod.replace_range(at..at + "#[cfg(test)]".len(), "");
        }
    }
    prod.lines().filter(|l| !l.trim_start().starts_with("//")).collect::<Vec<_>>().join("\n")
}

#[test]
fn no_call_site_decides_an_endpoint_on_its_own() {
    let root = workspace_root();
    let files = production_files(&root);
    assert!(files.len() > 100, "the walk found the workspace ({} files)", files.len());
    let mut failures = Vec::new();
    let mut seen_allowed = std::collections::BTreeSet::new();
    for file in &files {
        if SELF.contains(&file.as_str()) {
            continue;
        }
        let text = production_text(&std::fs::read_to_string(root.join(file)).unwrap());
        for idiom in FORBIDDEN {
            let count = text.matches(idiom.pattern).count();
            let allowed = ALLOWED.iter().find(|(f, p, _, _)| f == file && *p == idiom.pattern);
            match (count, allowed) {
                (0, None) => {}
                (n, Some((_, _, want, _))) if n == *want => {
                    seen_allowed.insert((file.clone(), idiom.pattern));
                }
                (n, Some((_, _, want, why))) => failures.push(format!(
                    "{file}: `{}` appears {n} time(s); the roster allows {want} ({why}). A new \
                     occurrence should call {} instead; a removed one should shrink the roster.",
                    idiom.pattern, idiom.route
                )),
                (n, None) => failures.push(format!(
                    "{file}: `{}` appears {n} time(s) in production code. Route it through {} \
                     (#2902: one endpoint/model resolver), or, if this file is a new home for \
                     the rule, add it to ALLOWED with the reason.",
                    idiom.pattern, idiom.route
                )),
            }
        }
    }
    for (file, pattern, _, why) in ALLOWED {
        if !seen_allowed.contains(&(file.to_string(), *pattern)) && !failures.iter().any(|f| f.starts_with(file)) {
            failures.push(format!("{file}: the roster allows `{pattern}` ({why}) but it no longer appears; drop the entry"));
        }
    }
    assert!(failures.is_empty(), "endpoint conformance:\n{}", failures.join("\n"));
}

/// The sweep can fail: a planted chat URL, `is_remote` test and host split
/// in otherwise-clean source are each counted, and a test module is not.
#[test]
fn the_sweep_counts_production_idioms_and_skips_test_modules() {
    let src = "fn a() -> String { format!(\"{b}/chat/completions\") }\n\
               fn c(pm: &P) -> bool { pm.is_remote() }\n\
               // a comment naming /chat/completions is not code\n\
               #[cfg(test)]\nmod tests {\n    fn t() { let _ = \"x/chat/completions\"; u.split(\"://\"); }\n}\n\
               fn d(u: &str) { u.split(\"://\"); }\n";
    let text = production_text(src);
    assert_eq!(text.matches("/chat/completions").count(), 1, "{text}");
    assert_eq!(text.matches(".is_remote()").count(), 1);
    assert_eq!(text.matches("split(\"://\")").count(), 1, "production code after the test module still counts");
    // (#2902 review C2) The URL split across arguments, and a presence test
    // on the model's endpoint.
    let more = production_text("fn e(b: &str) -> String { format!(\"{b}/{}\", \"chat/completions\") }\nfn f(pm: &P) -> bool { pm.endpoint.is_some() }\n");
    assert_eq!(more.matches("chat/completions").count(), 1);
    assert_eq!(more.matches(".endpoint.is_some()").count(), 1);
}
