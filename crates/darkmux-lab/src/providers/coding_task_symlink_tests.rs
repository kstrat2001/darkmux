//! (#3394) A run's sandbox is a clone of a fixture, and the clone keeps the
//! fixture's symlinks. Every host-side write `setup` makes into the sandbox
//! (`setupContent`, and the `sandboxSeed` copy) must land inside it, whatever
//! links the fixture holds: each shape below is refused before anything is
//! written, with the error naming the path, and the link's target unchanged.

use super::*;
use crate::workloads::types::{WorkloadManifest, WorkloadSource, WorkloadSpec};
use std::collections::BTreeMap;
use std::os::unix::fs::symlink;
use tempfile::TempDir;

fn spec_with(setup: &[(&str, &str)]) -> WorkloadSpec {
    WorkloadSpec {
        id: "coding".into(),
        provider: "coding-task".into(),
        description: None,
        role: None,
        prompt: Some("write tests".into()),
        prompt_file: None,
        sandbox_seed: None,
        setup_content: setup
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect(),
        requires_external_sandbox: false,
        requires_fixture: None,
        verify: None,
        expected: None,
        image: None,
        trials: None,
        task_timeout_seconds: None,
        chain_depths: None,
        reply_room: None,
        seed: None,
        extras: BTreeMap::new(),
    }
}

/// A workload dir (`base`), a sandbox already cloned from a fixture
/// (`sandbox`), and a folder outside both (`outside`) holding `target.txt`.
struct Scene {
    _tmp: TempDir,
    base: PathBuf,
    run: PathBuf,
    sandbox: PathBuf,
    outside: PathBuf,
}

fn scene() -> Scene {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().join("base");
    let sandbox = tmp.path().join("sandbox");
    let outside = tmp.path().join("outside");
    for d in [&base, &sandbox, &outside] {
        fs::create_dir_all(d).unwrap();
    }
    fs::write(outside.join("target.txt"), "operator's own file").unwrap();
    let run = tmp.path().join("run");
    Scene {
        _tmp: tmp,
        base,
        run,
        sandbox,
        outside,
    }
}

fn setup_err(s: &Scene, spec: WorkloadSpec) -> String {
    let loaded = LoadedWorkload {
        manifest: WorkloadManifest {
            schema_version: None,
            workload: spec,
        },
        base_dir: s.base.clone(),
        source: WorkloadSource::OnDisk,
        lab_inputs: None,
    };
    match CodingTaskProvider.setup(&loaded, &s.run, &s.sandbox) {
        Ok(()) => panic!("setup must refuse a write through a symlink"),
        Err(e) => format!("{e:#}"),
    }
}

fn assert_names_link(err: &str, link: &str) {
    assert!(err.contains("symlink"), "the refusal says why: {err}");
    assert!(
        err.contains(&format!("`{link}`")),
        "the refusal names the link `{link}`: {err}"
    );
}

fn assert_outside_untouched(s: &Scene) {
    assert_eq!(
        fs::read_to_string(s.outside.join("target.txt")).unwrap(),
        "operator's own file"
    );
    let names: Vec<_> = fs::read_dir(&s.outside)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(
        names,
        vec![std::ffi::OsString::from("target.txt")],
        "nothing new outside the sandbox"
    );
}

#[test]
fn setup_content_refuses_a_leaf_symlink_to_a_file_outside() {
    let s = scene();
    symlink(s.outside.join("target.txt"), s.sandbox.join("notes.txt")).unwrap();
    let err = setup_err(&s, spec_with(&[("notes.txt", "overwritten")]));
    assert_names_link(&err, "notes.txt");
    assert_outside_untouched(&s);
}

#[test]
fn setup_content_refuses_a_folder_symlink_to_a_dir_outside() {
    // The issue's own example: `scratch -> ~` plus a `scratch/.zshrc` key.
    let s = scene();
    symlink(&s.outside, s.sandbox.join("scratch")).unwrap();
    let err = setup_err(&s, spec_with(&[("scratch/.zshrc", "evil")]));
    assert_names_link(&err, "scratch");
    assert_outside_untouched(&s);
}

#[test]
fn setup_content_refuses_a_dangling_symlink() {
    let s = scene();
    symlink(s.outside.join("not-yet.txt"), s.sandbox.join("new.txt")).unwrap();
    let err = setup_err(&s, spec_with(&[("new.txt", "created through the link")]));
    assert_names_link(&err, "new.txt");
    assert_outside_untouched(&s);
}

#[test]
fn setup_content_refuses_a_chain_of_symlinks() {
    // `a -> b -> outside`, where `b` is itself a link inside the sandbox.
    let s = scene();
    symlink(&s.outside, s.sandbox.join("b")).unwrap();
    symlink("b", s.sandbox.join("a")).unwrap();
    let err = setup_err(&s, spec_with(&[("a/x.txt", "evil")]));
    assert_names_link(&err, "a");
    assert_outside_untouched(&s);
}

#[test]
fn setup_content_refuses_a_symlink_as_an_intermediate_component() {
    // A real folder, then a link, then folders `create_dir_all` would make
    // on the far side of the link.
    let s = scene();
    fs::create_dir_all(s.sandbox.join("deep")).unwrap();
    symlink(&s.outside, s.sandbox.join("deep/link")).unwrap();
    let err = setup_err(&s, spec_with(&[("deep/link/inner/x.txt", "evil")]));
    assert_names_link(&err, "deep/link");
    assert_outside_untouched(&s);
}

#[test]
fn setup_content_refuses_a_relative_link_back_into_the_fixture_checkout() {
    // A relative link out of the clone into the checkout it came from: a
    // write there changes what the next `lab run` reads.
    let s = scene();
    let checkout = s.outside.clone();
    symlink("../outside", s.sandbox.join("up")).unwrap();
    let err = setup_err(&s, spec_with(&[("up/target.txt", "poisoned fixture")]));
    assert_names_link(&err, "up");
    assert_eq!(
        fs::read_to_string(checkout.join("target.txt")).unwrap(),
        "operator's own file"
    );
}

#[test]
fn sandbox_seed_refuses_a_file_symlink_in_the_sandbox() {
    let s = scene();
    fs::create_dir_all(s.base.join("sandbox")).unwrap();
    fs::write(s.base.join("sandbox/notes.txt"), "seeded").unwrap();
    symlink(s.outside.join("target.txt"), s.sandbox.join("notes.txt")).unwrap();
    let err = setup_err(&s, spec_with(&[]));
    assert_names_link(&err, "notes.txt");
    assert_outside_untouched(&s);
}

#[test]
fn sandbox_seed_refuses_a_folder_symlink_in_the_sandbox() {
    let s = scene();
    fs::create_dir_all(s.base.join("sandbox/sub")).unwrap();
    fs::write(s.base.join("sandbox/sub/x.txt"), "seeded").unwrap();
    symlink(&s.outside, s.sandbox.join("sub")).unwrap();
    let err = setup_err(&s, spec_with(&[]));
    assert_names_link(&err, "sub");
    assert_outside_untouched(&s);
}

#[test]
fn setup_content_still_overwrites_a_real_file_and_keeps_its_mode() {
    // The ordinary path: a fixture's regular file is replaced in place, and
    // an executable stays executable.
    use std::os::unix::fs::PermissionsExt;
    let s = scene();
    fs::write(s.sandbox.join("run.sh"), "old").unwrap();
    fs::set_permissions(s.sandbox.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    let loaded = LoadedWorkload {
        manifest: WorkloadManifest {
            schema_version: None,
            workload: spec_with(&[("run.sh", "new"), ("a/b.txt", "made")]),
        },
        base_dir: s.base.clone(),
        source: WorkloadSource::OnDisk,
        lab_inputs: None,
    };
    CodingTaskProvider
        .setup(&loaded, &s.run, &s.sandbox)
        .unwrap();
    assert_eq!(fs::read_to_string(s.sandbox.join("run.sh")).unwrap(), "new");
    let mode = fs::metadata(s.sandbox.join("run.sh"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o755);
    assert_eq!(
        fs::read_to_string(s.sandbox.join("a/b.txt")).unwrap(),
        "made"
    );
}
