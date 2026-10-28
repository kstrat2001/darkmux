//! (#3363) A lab dispatch's system prompt is built only from pinned inputs:
//! the eval's own role files (or darkmux's compiled-in role, for a workload
//! bound to no fixture), darkmux's compiled-in preamble, and nothing from the
//! home folder. A non-lab dispatch is unchanged.

use super::*;
use crate::dispatch::LabInputs;
use darkmux_flow::payload::RoleInputSource;
use serial_test::serial;
use tempfile::TempDir;

/// Sets env vars for one test and puts back what was there.
struct Env(Vec<(&'static str, Option<String>)>);

impl Env {
    fn set(vars: &[(&'static str, &std::path::Path)]) -> Self {
        let saved = vars.iter().map(|(k, _)| (*k, std::env::var(k).ok())).collect();
        for (k, v) in vars {
            unsafe { std::env::set_var(k, v) };
        }
        Env(saved)
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        for (k, v) in &self.0 {
            match v {
                Some(v) => unsafe { std::env::set_var(k, v) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
    }
}

const ROLE: &str = "web-probe";
const STALE_HOME_MD: &str = "# web-probe\n\nSTALE-HOME-PROMPT: the 2026-10-24 copy left in the home folder.\n";
const PINNED_EVAL_MD: &str = "# web-probe\n\nPINNED-EVAL-PROMPT: the copy the eval's commit pins.\n";
const IDENTITY: &str = "OPERATOR-IDENTITY-3363";
const HOME_PREAMBLE: &str = "HOME-PREAMBLE-OVERRIDE-3363";

fn role_json(id: &str) -> String {
    serde_json::json!({
        "schema_version": "1.0",
        "id": id,
        "role_family": "specialist",
        "description": "probes a site",
        "tool_palette": {"allow": ["read"], "deny": []},
        "escalation_contract": "bail-with-explanation"
    })
    .to_string()
}

/// A home folder holding a stale `roles/web-probe.*`, an identity file and a
/// preamble override: everything a lab run must not read.
fn home_with_overrides() -> TempDir {
    let home = TempDir::new().unwrap();
    let roles = home.path().join("roles");
    std::fs::create_dir_all(&roles).unwrap();
    std::fs::write(roles.join(format!("{ROLE}.json")), role_json(ROLE)).unwrap();
    std::fs::write(roles.join(format!("{ROLE}.md")), STALE_HOME_MD).unwrap();
    std::fs::write(home.path().join("identity.md"), IDENTITY).unwrap();
    std::fs::write(home.path().join(crate::loader::PREAMBLE_OVERRIDE_FILE), HOME_PREAMBLE).unwrap();
    home
}

/// An eval checkout carrying `roles/web-probe.json` and `roles/web-probe.md`.
fn eval_with_role(md: &str) -> TempDir {
    let eval = TempDir::new().unwrap();
    let roles = eval.path().join("roles");
    std::fs::create_dir_all(&roles).unwrap();
    std::fs::write(roles.join(format!("{ROLE}.json")), role_json(ROLE)).unwrap();
    std::fs::write(roles.join(format!("{ROLE}.md")), md).unwrap();
    eval
}

/// A local dispatch with every optional input off.
fn probe_opts() -> DispatchOpts {
    DispatchOpts {
        finding_sites: None,
        allow_utility_model: false,
        remote_origin: None,
        live_channel: true,
        brief_refs: Vec::new(),
        workspace_read_only: false,
        record_context: None,
        resume_from: None,
        host_out: None,
        max_turns_override: None,
        timeout_override_seconds: None,
        role_id: String::new(),
        message: "probe".to_string(),
        session: crate::test_session("lab-role-3363"),
        timeout_seconds: 5,
        skip_preflight: true,
        json: true,
        workdir: None,
        phase_id: None,
        machine: None,
        wait: true,
        compaction: crate::dispatch::CompactionDispatchArgs::default(),
        profile_name: None,
        config_path: None,
        force_container: false,
        max_completion_tokens: None,
        image: None,
        model_base_url_override: None,
        step_id: None,
        system_prompt_override: None,
        lab_inputs: None,
    }
}

/// The eval's role, read once as `lab run` reads it.
fn eval_inputs(eval: &TempDir) -> LabInputs {
    LabInputs::load_eval(eval.path(), ROLE).expect("the eval carries its role")
}

/// Options that reach prompt assembly and stop at model selection: the
/// registry file does not exist (see `dispatch_appends_no_note_at_the_call_site_for_a_plain_workdir`).
fn opts(home: &TempDir, role: &str, lab_inputs: Option<LabInputs>) -> DispatchOpts {
    let mut o = probe_opts();
    o.role_id = role.to_string();
    o.config_path = Some(home.path().join("no-such-profiles-3363.json").display().to_string());
    o.lab_inputs = lab_inputs;
    o
}

/// The env every test here runs under: the home folder, and no identity
/// override from the developer's shell.
fn env_for(home: &TempDir) -> Env {
    let env = Env::set(&[("DARKMUX_HOME", home.path())]);
    unsafe { std::env::remove_var("DARKMUX_IDENTITY_PATH") };
    env
}

/// The system prompt `dispatch()` assembled, and the error it then stopped on.
fn dispatched_prompt(o: DispatchOpts) -> (Option<String>, String) {
    let mut err = String::new();
    let prompts = capture_system_prompts(|| {
        err = format!("{:#}", dispatch(o).expect_err("no registry: the dispatch stops at model selection"));
    });
    (prompts.last().cloned(), err)
}

/// The 2026-10-24 incident: a stale `~/.darkmux/roles/web-probe.md` replaced
/// the eval's pinned `web-probe.md` for 19 biz-scrape runs. A lab dispatch of
/// a workload bound to that eval must send the eval's bytes, and record the
/// eval's hashes.
#[test]
#[serial]
fn a_lab_dispatch_sends_the_evals_role_not_a_stale_copy_in_the_home_folder() {
    let home = home_with_overrides();
    let eval = eval_with_role(PINNED_EVAL_MD);
    let _env = env_for(&home);
    let lab = Some(eval_inputs(&eval));

    let (prompt, err) = dispatched_prompt(opts(&home, ROLE, lab.clone()));
    assert!(err.contains("model selection failed"), "the dispatch reaches the model, and no further: {err}");
    let prompt = prompt.expect("dispatch() assembled a system prompt");
    assert!(prompt.contains("PINNED-EVAL-PROMPT"), "the eval's role reaches the model: {prompt}");
    assert!(!prompt.contains("STALE-HOME-PROMPT"), "the home folder's copy never does: {prompt}");

    let (_, _, _, prov) = container_role(&opts(&home, ROLE, lab), false, &[]).unwrap();
    let role = prov.role.expect("a lab dispatch records its role");
    assert_eq!(role.id, ROLE);
    assert_eq!(role.source, RoleInputSource::Eval);
    assert_eq!(role.md_sha256, darkmux_flow::sha256_hex(PINNED_EVAL_MD.as_bytes()), "the eval's md, as loaded");
    assert_eq!(role.json_sha256, darkmux_flow::sha256_hex(role_json(ROLE).as_bytes()));
    assert_eq!(
        role.preamble_sha256.as_deref(),
        Some(darkmux_flow::sha256_hex(crate::loader::AUTONOMOUS_DISPATCH_PREAMBLE.as_bytes()).as_str()),
        "a specialist gets darkmux's compiled-in preamble"
    );
}

/// An eval that does not carry the role is refused when the role is read,
/// naming the missing file; with nothing loaded there is nothing to
/// dispatch, and the home folder's copy is not a fallback.
#[test]
#[serial]
fn an_eval_that_lacks_the_role_is_refused_naming_the_file() {
    let home = home_with_overrides();
    let eval = eval_with_role(PINNED_EVAL_MD);
    std::fs::remove_file(eval.path().join("roles").join(format!("{ROLE}.md"))).unwrap();
    let _env = env_for(&home);
    let err = format!("{:#}", LabInputs::load_eval(eval.path(), ROLE).unwrap_err());
    let missing = eval.path().join("roles").join(format!("{ROLE}.md"));
    assert!(err.contains(&missing.display().to_string()), "names the missing file: {err}");
    assert!(err.contains("the eval must carry its roles"), "says what to do: {err}");
}

/// The role a lab run loaded is the role its dispatch runs: a dispatch of
/// another role id with those inputs is refused, not run on either.
#[test]
#[serial]
fn a_lab_dispatch_of_another_role_than_the_loaded_one_is_refused() {
    let home = home_with_overrides();
    let eval = eval_with_role(PINNED_EVAL_MD);
    let _env = env_for(&home);
    let (prompt, err) = dispatched_prompt(opts(&home, "coder", Some(eval_inputs(&eval))));
    assert!(err.contains("loaded role `web-probe` from the eval, but this dispatch runs `coder`"), "{err}");
    assert_eq!(prompt, None);
}

/// (#3363 re-review) The role is read once, before the run writes anything
/// into its sandbox, and the dispatch uses those bytes. A workload's
/// `setupContent` writing `roles/coder.md`, a case-folded `Roles/...` (APFS
/// folds it onto `roles/`), or a path through a `cfg -> roles` link the
/// fixture carries, lands in the sandbox (the workdir) and changes nothing
/// the dispatch sends or records.
#[test]
#[serial]
fn writes_into_the_sandbox_after_the_role_is_read_cannot_change_the_prompt() {
    let home = home_with_overrides();
    let sandbox = eval_with_role(PINNED_EVAL_MD);
    std::os::unix::fs::symlink("roles", sandbox.path().join("cfg")).unwrap();
    let _env = env_for(&home);
    let lab = eval_inputs(&sandbox);
    for (n, key) in [format!("roles/{ROLE}.md"), format!("Roles/{ROLE}.md"), format!("cfg/{ROLE}.md")].iter().enumerate() {
        // What the coding-task provider's setup does with a setupContent key.
        let evil = format!("# web-probe\n\nREPLACED-BY-SETUP-{n}\n");
        let target = sandbox.path().join(key);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, &evil).unwrap();
        // A case-sensitive filesystem (Linux CI) keeps `Roles/` apart; macOS folds it onto `roles/`.
        let folds_case = sandbox.path().join("ROLES").exists();
        if folds_case || !key.starts_with("Roles") {
            assert_eq!(std::fs::read_to_string(sandbox.path().join(format!("roles/{ROLE}.md"))).unwrap(), evil, "precondition: {key} reached roles/");
        }

        let mut o = opts(&home, ROLE, Some(lab.clone()));
        o.workdir = Some(sandbox.path().to_path_buf());
        let (prompt, _) = dispatched_prompt(o);
        let prompt = prompt.expect("dispatch() assembled a system prompt");
        assert!(prompt.contains("PINNED-EVAL-PROMPT") && !prompt.contains("REPLACED-BY-SETUP"), "{key}: {prompt}");
        let mut o = opts(&home, ROLE, Some(lab.clone()));
        o.workdir = Some(sandbox.path().to_path_buf());
        let (_, _, _, prov) = container_role(&o, false, &[]).unwrap();
        assert_eq!(prov.role.unwrap().md_sha256, darkmux_flow::sha256_hex(PINNED_EVAL_MD.as_bytes()), "{key}");
    }
}

/// A role manifest in an eval is a user file: an unknown key refuses it the
/// way every role manifest is refused (contract 7).
#[test]
#[serial]
fn an_eval_role_with_an_unknown_key_is_refused_like_any_role_manifest() {
    let home = home_with_overrides();
    let eval = eval_with_role(PINNED_EVAL_MD);
    let mut doc: serde_json::Value = serde_json::from_str(&role_json(ROLE)).unwrap();
    doc["tool_pallete"] = serde_json::json!({"allow": []});
    std::fs::write(eval.path().join("roles").join(format!("{ROLE}.json")), doc.to_string()).unwrap();
    let _env = env_for(&home);

    let err = LabInputs::load_eval(eval.path(), ROLE).unwrap_err();
    let err = format!("{err:#}");
    assert!(err.contains("tool_pallete") && err.contains("tool_palette"), "names the key and the closest one: {err}");
}

/// A workload bound to no fixture runs on darkmux's compiled-in role: a role
/// that exists only in the home folder is refused, never read.
#[test]
#[serial]
fn a_builtin_lab_dispatch_refuses_a_role_only_the_home_folder_has() {
    let home = home_with_overrides();
    let _env = env_for(&home);
    let err = crate::dispatch::check_lab_role(&LabInputs::Builtin, ROLE).unwrap_err();
    let err = format!("{err:#}");
    assert!(err.contains("`web-probe` is not one of darkmux's built-in roles"), "{err}");
    crate::dispatch::check_lab_role(&LabInputs::Builtin, "coder").expect("a built-in role is fine");
}

/// A lab dispatch of a built-in workload gets the compiled-in role and
/// preamble, and none of the home folder's identity, role override or
/// preamble override.
#[test]
#[serial]
fn a_builtin_lab_dispatch_leaves_out_identity_and_every_home_override() {
    let home = home_with_overrides();
    std::fs::write(home.path().join("roles/coder.md"), STALE_HOME_MD).unwrap();
    let _env = env_for(&home);

    let (prompt, _) = dispatched_prompt(opts(&home, "coder", Some(LabInputs::Builtin)));
    let prompt = prompt.expect("dispatch() assembled a system prompt");
    let builtin_md = crate::loader::BUILTIN_ROLE_PROMPTS.iter().find(|(id, _)| *id == "coder").unwrap().1;
    assert!(prompt.contains(builtin_md.trim_end()), "the compiled-in coder prompt");
    assert!(prompt.starts_with(crate::loader::AUTONOMOUS_DISPATCH_PREAMBLE.trim_end()), "the compiled-in preamble");
    assert!(!prompt.contains(IDENTITY), "no operator identity in a lab prompt: {prompt}");
    assert!(!prompt.contains(HOME_PREAMBLE), "no preamble override: {prompt}");
    assert!(!prompt.contains("STALE-HOME-PROMPT"), "no home role override: {prompt}");

    let (_, _, _, prov) = container_role(&opts(&home, "coder", Some(LabInputs::Builtin)), false, &[]).unwrap();
    assert_eq!(prov.additions, darkmux_flow::payload::PromptAdditions::default(), "every addition off");
    assert_eq!(prov.role.expect("recorded").source, RoleInputSource::Builtin);
}

/// A lab dispatch gets no workspace note, even on a workdir that would earn
/// a non-lab dispatch one.
#[test]
#[serial]
fn a_lab_dispatch_gets_no_workspace_note() {
    let home = home_with_overrides();
    let _env = env_for(&home);
    let workdir = TempDir::new().unwrap();
    let split = [darkmux_types::workdir::SplitGitdir {
        checkout: workdir.path().to_path_buf(),
        target: workdir.path().join("../main/.git/worktrees/x"),
        kind: darkmux_types::workdir::GitdirPointerKind::Worktree,
        superproject: None,
    }];
    let mut o = opts(&home, "coder", Some(LabInputs::Builtin));
    o.workdir = Some(workdir.path().to_path_buf());
    let (_, prompt, _, prov) = container_role(&o, false, &split).unwrap();
    assert!(!prompt.contains("<workspace-note>"), "{prompt}");
    assert!(!prov.additions.workspace_notes);

    o.lab_inputs = None;
    let (_, prompt, _, prov) = container_role(&o, false, &split).unwrap();
    assert!(prompt.contains("<workspace-note>"), "a non-lab dispatch still gets it: {prompt}");
    assert!(prov.additions.workspace_notes, "and records it");
}

/// A non-lab dispatch is unchanged: the home folder's role, its preamble
/// override and the operator identity all still apply, and are recorded.
#[test]
#[serial]
fn a_non_lab_dispatch_still_gets_identity_and_the_home_overrides() {
    let home = home_with_overrides();
    let _env = env_for(&home);

    let (prompt, _) = dispatched_prompt(opts(&home, ROLE, None));
    let prompt = prompt.expect("dispatch() assembled a system prompt");
    assert_eq!(
        prompt,
        format!("{HOME_PREAMBLE}\n\n{STALE_HOME_MD}\n\n---\n\n## About the operator\n\n{IDENTITY}\n"),
        "byte for byte what a non-lab dispatch assembled before #3363"
    );

    let (_, _, _, prov) = container_role(&opts(&home, ROLE, None), false, &[]).unwrap();
    assert!(prov.additions.identity && prov.additions.role_override && prov.additions.preamble_override);
    assert!(!prov.additions.workspace_notes);
    assert_eq!(prov.role, None, "only a lab dispatch records the role block");
}

/// The hosted one-call path drops identity for a lab dispatch on a brain
/// that stays on this machine, where a non-lab dispatch gets it.
#[test]
#[serial]
fn the_hosted_path_leaves_identity_out_of_a_lab_prompt() {
    let home = home_with_overrides();
    let eval = eval_with_role(PINNED_EVAL_MD);
    let _env = env_for(&home);
    let lab = opts(&home, ROLE, Some(eval_inputs(&eval)));
    let resolved = crate::role_inputs::resolve_role(&lab).unwrap().unwrap();
    let (prompt, prov) = hosted_system_prompt(&lab, &resolved, false).unwrap();
    assert!(prompt.contains("PINNED-EVAL-PROMPT") && !prompt.contains(IDENTITY) && !prompt.contains(HOME_PREAMBLE), "{prompt}");
    assert_eq!(prov.role.expect("recorded").md_sha256, darkmux_flow::sha256_hex(PINNED_EVAL_MD.as_bytes()));

    let plain = opts(&home, ROLE, None);
    let resolved = crate::role_inputs::resolve_role(&plain).unwrap().unwrap();
    let (prompt, prov) = hosted_system_prompt(&plain, &resolved, false).unwrap();
    assert!(prompt.contains("STALE-HOME-PROMPT") && prompt.contains(IDENTITY), "{prompt}");
    assert!(prov.additions.identity);
}

/// The local single-shot path drops identity for a lab dispatch too.
#[test]
#[serial]
fn the_local_single_shot_path_leaves_identity_out_of_a_lab_prompt() {
    let home = home_with_overrides();
    let _env = env_for(&home);
    let lab = opts(&home, "pr-reviewer", Some(LabInputs::Builtin));
    let resolved = crate::role_inputs::resolve_role(&lab).unwrap().unwrap();
    let (prompt, prov) = local_single_shot_system_prompt(&lab, &resolved).unwrap();
    assert!(!prompt.contains(IDENTITY), "{prompt}");
    assert!(!prov.additions.identity);

    let plain = opts(&home, "pr-reviewer", None);
    let resolved = crate::role_inputs::resolve_role(&plain).unwrap().unwrap();
    let (prompt, prov) = local_single_shot_system_prompt(&plain, &resolved).unwrap();
    assert!(prompt.contains(IDENTITY), "{prompt}");
    assert!(prov.additions.identity);
}

/// The `dispatch.start` stamp: the assembled prompt's hash on every
/// dispatch, the additions, and the role block when there is one.
#[test]
fn the_start_record_carries_the_prompt_hash_the_additions_and_the_role() {
    let mut payload = DispatchStartPayload::default();
    let prov = crate::role_inputs::PromptProvenance {
        additions: darkmux_flow::payload::PromptAdditions { identity: true, ..Default::default() },
        role: None,
    };
    crate::role_inputs::stamp_prompt_inputs(&mut payload, "the system prompt", &prov);
    assert_eq!(payload.system_sha256.as_deref(), Some(darkmux_flow::sha256_hex(b"the system prompt").as_str()));
    assert_eq!(payload.prompt_additions, Some(prov.additions));
    assert_eq!(payload.role, None);
    let v = serde_json::to_value(&payload).unwrap();
    assert_eq!(v["prompt_additions"]["identity"], true);
    assert!(v.get("role").is_none(), "a non-lab record carries no role block");
}

/// An eval's role file is named for its id: a manifest declaring another id
/// is refused rather than run under the wrong name.
#[test]
#[serial]
fn an_eval_role_whose_id_differs_from_its_file_name_is_refused() {
    let home = home_with_overrides();
    let eval = eval_with_role(PINNED_EVAL_MD);
    std::fs::write(eval.path().join("roles").join(format!("{ROLE}.json")), role_json("another-role")).unwrap();
    let _env = env_for(&home);
    let err = LabInputs::load_eval(eval.path(), ROLE).unwrap_err();
    let err = format!("{err:#}");
    assert!(err.contains("declares id `another-role`, but the workload runs `web-probe`"), "{err}");
}

/// A role that is not a specialist gets no preamble, and its record says so
/// with a `null` preamble hash rather than leaving the key out.
#[test]
#[serial]
fn a_utility_eval_role_gets_no_preamble_and_records_a_null_hash() {
    let home = home_with_overrides();
    let eval = eval_with_role(PINNED_EVAL_MD);
    let mut doc: serde_json::Value = serde_json::from_str(&role_json(ROLE)).unwrap();
    doc["role_family"] = serde_json::json!("utility");
    std::fs::write(eval.path().join("roles").join(format!("{ROLE}.json")), doc.to_string()).unwrap();
    let _env = env_for(&home);

    let o = opts(&home, ROLE, Some(eval_inputs(&eval)));
    let (_, prompt, _, prov) = container_role(&o, false, &[]).unwrap();
    assert_eq!(prompt, PINNED_EVAL_MD, "the eval's prompt alone, byte for byte");
    let role = prov.role.clone().expect("recorded");
    assert_eq!(role.preamble_sha256, None);
    let mut payload = DispatchStartPayload::default();
    crate::role_inputs::stamp_prompt_inputs(&mut payload, &prompt, &prov);
    let v = serde_json::to_value(&payload).unwrap();
    assert!(v["role"]["preamble_sha256"].is_null() && v["role"].get("preamble_sha256").is_some(), "{v}");
    assert_eq!(v["role"]["source"], "eval");
}

/// The container path's `dispatch.start` payload is built by
/// `dispatch_start_payload`: the prompt's hash sits beside its length, and a
/// lab dispatch's role block rides along.
#[test]
#[serial]
fn the_container_start_payload_carries_the_hash_beside_system_chars() {
    let home = home_with_overrides();
    let eval = eval_with_role(PINNED_EVAL_MD);
    let _env = env_for(&home);
    let o = opts(&home, ROLE, Some(eval_inputs(&eval)));
    let (_, prompt, _, prov) = container_role(&o, false, &[]).unwrap();
    let payload =
        dispatch_start_payload("img", "msg", &prompt, std::path::Path::new("/ws"), false, None, None, None, &[], &prov)
            .unwrap();
    assert_eq!(payload.system_chars, Some(prompt.chars().count() as u64));
    assert_eq!(payload.system_sha256, Some(darkmux_flow::sha256_hex(prompt.as_bytes())));
    assert_eq!(payload.prompt_additions, Some(darkmux_flow::payload::PromptAdditions::default()));
    assert_eq!(payload.role.expect("a lab dispatch's role block").md_sha256, darkmux_flow::sha256_hex(PINNED_EVAL_MD.as_bytes()));
}

/// The hosted one-call path writes the same fields on its `dispatch.start`
/// record.
#[test]
#[serial]
fn the_hosted_start_record_carries_the_prompt_hash_and_the_role() {
    let (base_url, _rx) = one_shot_http_mock(r#"{"choices":[{"message":{"content":"ok"}}],"usage":{"prompt_tokens":2,"completion_tokens":1}}"#);
    let home = TempDir::new().unwrap();
    let flows = TempDir::new().unwrap();
    let _env = Env::set(&[("DARKMUX_HOME", home.path()), ("DARKMUX_FLOWS_DIR", flows.path())]);
    unsafe { std::env::remove_var("DARKMUX_REDIS_URL") };
    let session = crate::test_session(&format!("hosted-prompt-inputs-{}", std::process::id()));
    let mut o = probe_opts();
    o.role_id = "pr-reviewer".to_string();
    o.session = session.clone();
    let prov = crate::role_inputs::PromptProvenance {
        additions: Default::default(),
        role: Some(darkmux_flow::payload::DispatchRoleInputs {
            id: "pr-reviewer".into(),
            source: RoleInputSource::Builtin,
            json_sha256: "j".into(),
            md_sha256: "m".into(),
            preamble_sha256: None,
        }),
    };
    let pm = darkmux_types::ProfileModel::hosted_for_test("gpt-remote", None, serde_json::json!({"url": base_url}));
    let target = crate::target::target_for("p".into(), Default::default(), pm).unwrap();
    dispatch_unmanaged(&o, &darkmux_types::execution_id::ExecutionId::mint(), "the system prompt", &prov, &target)
        .expect("the mock answers");
    let records = drain_flow_records_for_session(flows.path(), &session);
    let start = records.iter().find(|r| r["action"] == "dispatch.start").expect("a start bookend");
    let p = &start["payload"];
    assert_eq!(p["system_sha256"], darkmux_flow::sha256_hex(b"the system prompt"), "{p}");
    assert_eq!(p["prompt_additions"]["identity"], false, "{p}");
    assert_eq!(p["role"]["source"], "builtin", "{p}");
}

/// An eval role file that is a symlink is refused: the sandbox clone keeps
/// links, so a link to the home folder's stale copy would otherwise deliver
/// exactly the bytes this module keeps out.
#[test]
#[serial]
fn a_symlinked_eval_role_file_is_refused() {
    let home = home_with_overrides();
    let eval = TempDir::new().unwrap();
    let roles = eval.path().join("roles");
    std::fs::create_dir_all(&roles).unwrap();
    for ext in ["json", "md"] {
        std::os::unix::fs::symlink(home.path().join(format!("roles/{ROLE}.{ext}")), roles.join(format!("{ROLE}.{ext}")))
            .unwrap();
    }
    let _env = env_for(&home);
    let err = format!("{:#}", LabInputs::load_eval(eval.path(), ROLE).unwrap_err());
    assert!(err.contains(&format!("roles/{ROLE}.json` is a symlink")), "{err}");
}

/// A `roles/` folder that is itself a symlink is refused the same way.
#[test]
#[serial]
fn a_symlinked_eval_roles_folder_is_refused() {
    let home = home_with_overrides();
    let eval = TempDir::new().unwrap();
    std::os::unix::fs::symlink(home.path().join("roles"), eval.path().join("roles")).unwrap();
    let _env = env_for(&home);
    let err = LabInputs::load_eval(eval.path(), ROLE).unwrap_err();
    let err = format!("{err:#}");
    assert!(err.contains("roles` is a symlink") && err.contains("real folder"), "{err}");
}

/// The window a lab run records is the one its dispatch's model selection
/// makes from the eval's role: here the eval's `web-probe` asks for coding
/// (the 128k model) while the home folder's asks for nothing (the default
/// 32k model).
#[test]
#[serial]
fn the_recorded_lab_window_follows_the_evals_role_not_the_home_folders() {
    let home = home_with_overrides();
    let eval = eval_with_role(PINNED_EVAL_MD);
    let mut doc: serde_json::Value = serde_json::from_str(&role_json(ROLE)).unwrap();
    doc["skills"] = serde_json::json!(["coding"]);
    std::fs::write(eval.path().join("roles").join(format!("{ROLE}.json")), doc.to_string()).unwrap();
    let pf = home.path().join("profiles-3363-window.json");
    std::fs::write(
        &pf,
        r#"{"profiles":{"mixed":{"default_model":"generalist","models":[
                {"id":"generalist","n_ctx":32000,"capabilities":{"reasoning":1.0}},
                {"id":"codestar","n_ctx":128000,"capabilities":{"code":1.0}}
            ]}},
            "default_profile":"mixed"}"#,
    )
    .unwrap();
    let _env = env_for(&home);
    let lab = eval_inputs(&eval);
    assert_eq!(dispatch_window(ROLE, Some("mixed"), pf.to_str(), true).unwrap(), Some(32_000), "precondition: the home role");
    assert_eq!(lab_dispatch_window(&lab, ROLE, Some("mixed"), pf.to_str()).unwrap(), Some(128_000));
}

/// An eval role file past the role-manifest size cap is refused, not read.
#[test]
#[serial]
fn an_oversized_eval_role_file_is_refused() {
    let home = home_with_overrides();
    let eval = eval_with_role(&"x".repeat(darkmux_types::user_files::MAX_USER_FILE_BYTES as usize + 1));
    let _env = env_for(&home);
    let err = LabInputs::load_eval(eval.path(), ROLE).unwrap_err();
    assert!(format!("{err:#}").contains("byte cap for a role file"), "{err:#}");
}
