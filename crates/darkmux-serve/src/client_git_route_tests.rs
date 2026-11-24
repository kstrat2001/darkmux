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
