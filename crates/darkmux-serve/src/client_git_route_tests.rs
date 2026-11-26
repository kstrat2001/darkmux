use super::*;

#[test]
fn askpass_answers_only_github_prompts() {
    assert_eq!(askpass_answer("Username for 'https://github.com': ", "t0k"), Some("x-access-token".into()));
    assert_eq!(askpass_answer("Password for 'https://x-access-token@github.com': ", "t0k"), Some("t0k".into()));
    for other in [
        "Password for 'https://x-access-token@gitlab.com': ",
        "Password for 'https://github.com.evil.example': ",
        "Password for 'http://github.com': ",
        "Password for 'https://evil.example/github.com': ",
        "Passphrase for key '/Users/me/.ssh/id_ed25519': ",
        "",
    ] {
        assert_eq!(askpass_answer(other, "t0k"), None, "{other}");
    }
    assert_eq!(askpass_answer("Password for 'https://github.com': ", ""), None, "no token, no answer");
}

#[test]
fn a_token_never_shows_in_debug_or_in_git_text() {
    let t: GitToken = serde_json::from_value(serde_json::json!("ghs_abc123")).unwrap();
    assert!(!format!("{t:?}").contains("ghs_abc123"));
    assert_eq!(scrub("fatal: https://x:ghs_abc123@github.com", Some(&t)), "fatal: https://x:<token>@github.com");
}

#[test]
fn a_commit_id_is_7_to_40_lowercase_hex() {
    assert!(valid_commit("abcdef0"));
    assert!(valid_commit(&"a".repeat(40)));
    for bad in ["abc", "--hard", "ABCDEF0", "HEAD~1", &"a".repeat(41)] {
        assert!(!valid_commit(bad), "{bad}");
    }
}

fn net(stderr: &str) -> crate::client_workspace::NetOutput {
    use std::os::unix::process::ExitStatusExt;
    crate::client_workspace::NetOutput { status: std::process::ExitStatus::from_raw(128 << 8), stdout: String::new(), stderr: stderr.to_string() }
}

#[test]
fn an_authentication_failure_is_told_apart_from_other_failures() {
    for err in [
        "remote: Invalid username or password.\nfatal: Authentication failed for 'https://github.com/o/r.git/'",
        "fatal: could not read Username for 'https://github.com': terminal prompts disabled",
        "fatal: unable to access 'https://github.com/o/r.git/': The requested URL returned error: 403",
    ] {
        assert!(auth_failed(&net(err)), "{err}");
    }
    for err in ["fatal: unable to access 'https://github.com/o/r.git/': Could not resolve host: github.com", "error: failed to push some refs"] {
        assert!(!auth_failed(&net(err)), "{err}");
    }
}

#[test]
fn git_with_nothing_on_stderr_still_says_how_it_exited() {
    let said = why(&net(""), None);
    assert!(said.starts_with("git exited with"), "{said}");
    let t: GitToken = serde_json::from_value(serde_json::json!("ghs_t0k")).unwrap();
    let said = why(&net("fatal: https://x:ghs_t0k@github.com refused\n\n"), Some(&t));
    assert!(said.contains("refused") && !said.contains("ghs_t0k"), "{said}");
}

#[test]
fn askpass_writes_the_answer_or_fails() {
    let mut out = Vec::new();
    let code = askpass_reply(Some("Password for 'https://x-access-token@github.com': ".into()), Some("t0k".into()), &mut out);
    assert_eq!(code, std::process::ExitCode::SUCCESS);
    assert_eq!(String::from_utf8(out).unwrap(), "t0k\n");
    let mut out = Vec::new();
    assert_eq!(askpass_reply(Some("Password for 'https://gitlab.com': ".into()), Some("t0k".into()), &mut out), std::process::ExitCode::FAILURE);
    assert!(out.is_empty(), "nothing for another host");
    assert_eq!(askpass_reply(None, None, &mut Vec::new()), std::process::ExitCode::FAILURE);
    assert_eq!(askpass_answer("Token for 'https://github.com': ", "t0k"), None, "only the username and password prompts");
    assert_eq!(askpass_answer("Password for https://github.com", "t0k"), None, "no quoted address");
}
