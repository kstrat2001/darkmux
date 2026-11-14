use super::*;

/// An executable file at `path` holding `text`.
fn program(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn link(target: &Path, at: &Path) {
    std::fs::create_dir_all(at.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(target, at).unwrap();
}

/// A fake host shaped like a real one: nvm's node (npm a link to a script
/// asking `env` for node), and beside it the real-world installs whose
/// folders must never be granted whole: `~/.cargo`, `~/.lmstudio` (with
/// credentials), `~/.grok` (with auth.json), `~/.bun`, and a `~/.local/bin`
/// link into a git repository (`~/configs`). A "system" folder the sandbox
/// reads with no grant holds `sh` and `echo`.
struct FakeHost {
    _dir: tempfile::TempDir,
    home: PathBuf,
    nvm: PathBuf,
    system: PathBuf,
}

fn a_fake_host() -> FakeHost {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let home = root.join("home");
    let nvm = home.join(".nvm/versions/node/v24.16.0");
    program(&nvm.join("bin/node"), "\x7fELF");
    program(&nvm.join("lib/node_modules/npm/bin/npm-cli.js"), "#!/usr/bin/env node\n");
    link(Path::new("../lib/node_modules/npm/bin/npm-cli.js"), &nvm.join("bin/npm"));
    program(&home.join(".cargo/bin/cargo"), "\x7fELF");
    program(&home.join(".lmstudio/bin/lms"), "\x7fELF");
    std::fs::create_dir_all(home.join(".lmstudio/credentials")).unwrap();
    std::fs::write(home.join(".lmstudio/credentials/key"), "secret").unwrap();
    program(&home.join(".grok/bin/agent"), "\x7fELF");
    std::fs::write(home.join(".grok/auth.json"), "{}").unwrap();
    program(&home.join(".bun/bin/bun"), "\x7fELF");
    std::fs::create_dir_all(home.join("configs/.git")).unwrap();
    program(&home.join("configs/bin/de-preview"), "\x7fELF");
    program(&home.join("configs/tool"), "\x7fELF");
    link(&home.join("configs/bin/de-preview"), &home.join(".local/bin/de-preview"));
    let system = root.join("system/bin");
    program(&system.join("sh"), "\x7fELF");
    program(&system.join("echo"), "\x7fELF");
    FakeHost { _dir: dir, home, nvm, system }
}

impl FakeHost {
    fn path_var(&self) -> String {
        let h = |p: &str| self.home.join(p).display().to_string();
        [h(".cargo/bin"), self.nvm.join("bin").display().to_string(), h(".lmstudio/bin"), h(".grok/bin"), h(".bun/bin"), h(".local/bin"), h("configs"), self.system.display().to_string(), "relative/bin".into(), String::new()].join(":")
    }

    fn resolve(&self, commands: &[&str], workdir: Option<&Path>) -> Result<Toolchain, String> {
        let system = self.system.clone();
        let reads = move |p: &Path| p.starts_with(&system);
        let path = self.path_var();
        resolve(&Ask { commands: commands.to_vec(), workdir, path_var: &path, home: Some(&self.home), reads: &reads, managed: Vec::new(), placed: "the `seatbelt` sandbox".into(), runs_with: "`darkmux client serve`" })
    }
}

/// (#3112) npm on nvm grants nvm's version folder (npm's link target and
/// the node its script asks `env` for are in it), never `~/.nvm` or the
/// home; the sandbox's PATH keeps the host's order, dropping what it cannot
/// read and every relative folder.
#[test]
fn nvm_npm_grants_its_version_folder_only() {
    let host = a_fake_host();
    let got = host.resolve(&["PORT=1 exec npm run dev && sh -c true"], None).unwrap();
    assert_eq!(got.read_only, vec![host.nvm.clone()]);
    assert_eq!(got.path.as_deref(), Some(format!("{}:{}", host.nvm.join("bin").display(), host.system.display()).as_str()));
    assert!(got.identity.contains("node=") && got.identity.contains("npm-cli.js"), "{}", got.identity);
}

/// (#3112 review) Real-world installs grant only the program's own folder
/// (and its link target's): `~/.cargo/bin`, `~/.lmstudio/bin` (never its
/// credentials), `~/.grok/bin` (never auth.json), `~/.bun/bin`, and for a
/// `~/.local/bin` link into a git repository, `~/.local/bin` and the
/// repository's `bin`, never the repository's root.
#[test]
fn real_world_installs_grant_only_their_own_folders() {
    let host = a_fake_host();
    let h = |p: &str| host.home.join(p);
    for (program, want) in [
        ("cargo", vec![h(".cargo/bin")]),
        ("lms", vec![h(".lmstudio/bin")]),
        ("agent", vec![h(".grok/bin")]),
        ("bun", vec![h(".bun/bin")]),
        ("de-preview", vec![h(".local/bin"), h("configs/bin")]),
    ] {
        let got = host.resolve(&[program], None).unwrap();
        assert_eq!(got.read_only, want, "{program}");
        for secret in [h(".lmstudio/credentials/key"), h(".grok/auth.json"), h("configs/.git"), h(".cargo/env")] {
            assert!(!got.read_only.iter().any(|g| secret.starts_with(g)), "{program} grants {}", secret.display());
        }
    }
}

/// A program whose folder is too wide to grant (a git repository's root,
/// the root, the home, a top-level dot-folder of the home) is granted as
/// its file alone.
#[test]
fn a_program_in_a_folder_too_wide_to_grant_is_granted_alone() {
    let host = a_fake_host();
    let got = host.resolve(&["tool"], None).unwrap();
    assert_eq!(got.read_only, vec![host.home.join("configs/tool")], "a git repository's root");
    assert_eq!(grant_for(Path::new("/tool"), None), Ok(PathBuf::from("/tool")), "the root, with no home known");
    assert_eq!(grant_for(&host.home.join("devserve"), Some(&host.home)), Ok(host.home.join("devserve")), "the home");
    assert_eq!(grant_for(&host.home.join(".grok/agent"), Some(&host.home)), Ok(host.home.join(".grok/agent")), "a top-level dot-folder");
}

/// Known layouts widen to their version folder: fnm, pyenv, asdf, Volta's
/// image, a Homebrew keg. A version folder that would be the home is not.
#[test]
fn known_layouts_widen_to_their_version_folder() {
    for (bin, want) in [
        ("/h/.local/share/fnm/node-versions/v22.1.0/installation/bin", "/h/.local/share/fnm/node-versions/v22.1.0/installation"),
        ("/h/.pyenv/versions/3.12.1/bin", "/h/.pyenv/versions/3.12.1"),
        ("/h/.asdf/installs/nodejs/22.1.0/bin", "/h/.asdf/installs/nodejs/22.1.0"),
        ("/h/.volta/tools/image/node/22.1.0/bin", "/h/.volta/tools/image/node/22.1.0"),
        ("/opt/homebrew/Cellar/node/24.1.0/bin", "/opt/homebrew/Cellar/node/24.1.0"),
        ("/h/.nvm/versions/node/v24.16.0/bin", "/h/.nvm/versions/node/v24.16.0"),
    ] {
        assert_eq!(grant_for(&Path::new(bin).join("node"), Some(Path::new("/h"))), Ok(PathBuf::from(want)), "{bin}");
    }
    assert_eq!(grant_for(Path::new("/h/.cargo/bin/cargo"), Some(Path::new("/h"))), Ok(PathBuf::from("/h/.cargo/bin")));
}

/// (#3112 review, MINOR 8) A version manager's shim is refused, naming it
/// and the fix: it would look for its version in the manager's files and
/// the home, which the sandbox cannot read.
#[test]
fn a_version_managers_shim_is_refused() {
    let host = a_fake_host();
    program(&host.home.join(".pyenv/shims/python3"), "#!/usr/bin/env bash\nexec pyenv exec python3\n");
    program(&host.home.join(".volta/bin/volta-shim"), "\x7fELF");
    link(&host.home.join(".volta/bin/volta-shim"), &host.home.join(".volta/bin/node"));
    let path = format!("{}:{}:{}", host.home.join(".pyenv/shims").display(), host.home.join(".volta/bin").display(), host.path_var());
    let reads = |_: &Path| false;
    for program in ["python3", "node"] {
        let why = resolve(&Ask { commands: vec![program], workdir: None, path_var: &path, home: Some(&host.home), reads: &reads, managed: Vec::new(), placed: "x".into(), runs_with: "y" }).unwrap_err();
        assert!(why.contains("shim") && why.contains("real install"), "{program}: {why}");
    }
}

/// A program the command runs that is not on PATH is refused, naming it,
/// where darkmux looked, whose PATH it is, and the fix.
#[test]
fn a_missing_program_is_refused_naming_what_was_looked_for() {
    let host = a_fake_host();
    let why = host.resolve(&["pnpm dev"], None).unwrap_err();
    for part in ["`pnpm`", &host.nvm.join("bin").display().to_string(), "`darkmux client serve`", "seatbelt", "on PATH"] {
        assert!(why.contains(part), "missing `{part}` in: {why}");
    }
}

/// (#3112 review, MINOR 5) Only absolute PATH folders count: a relative one
/// or an empty one (the current folder) is dropped, once each.
#[test]
fn only_absolute_path_folders_count() {
    assert_eq!(path_dirs("a:/b::rel/c:/b:/d"), [PathBuf::from("/b"), PathBuf::from("/d")]);
}

/// (#3112 review, MINOR 4) Quotes hold their separators: what a quoted
/// argument says is never a program. `env` alone is the program; a
/// redirection and its target are skipped.
#[test]
fn quotes_hold_their_separators() {
    let on = |p: &str| Program::OnPath(p.into());
    assert_eq!(programs_of(r#"node -e "require('./server').listen(3000)""#), [on("node")]);
    assert_eq!(programs_of(r#"python3 -c "import app; app.run()""#), [on("python3")]);
    assert_eq!(programs_of(r#"echo "x; lms" && echo 'a | b'"#), [on("echo")]);
    assert_eq!(
        programs_of("env > env.txt; cat 'x y' > leak.txt 2>&1; PORT=1 exec python3 -m http.server && ./run.sh | $X || env -i A=1 node a.js & (nohup npx vite) >out 2> err"),
        [on("env"), on("cat"), on("python3"), Program::At("./run.sh".into()), on("node"), on("npx")]
    );
    assert_eq!(programs_of(r#"FOO="a b" npm run dev"#), [on("npm")]);
}

/// (#3112 review, MINOR 4) A workspace script's interpreter is resolved and
/// granted when it is a known runtime (`./node_modules/.bin/vite` asks for
/// node); one that names any other program grants nothing.
#[test]
fn a_workspace_scripts_runtime_is_granted_and_nothing_else() {
    let host = a_fake_host();
    let ws = tempfile::TempDir::new().unwrap();
    program(&ws.path().join("node_modules/vite/bin/vite.js"), "#!/usr/bin/env node\n");
    link(Path::new("../vite/bin/vite.js"), &ws.path().join("node_modules/.bin/vite"));
    program(&ws.path().join("evil.sh"), "#!/usr/bin/env lms\n");
    let got = host.resolve(&["./node_modules/.bin/vite --host"], Some(ws.path())).unwrap();
    assert_eq!(got.read_only, vec![host.nvm.clone()]);
    let got = host.resolve(&["./evil.sh"], Some(ws.path())).unwrap();
    assert!(got.read_only.is_empty(), "a workspace script widened the grant: {:?}", got.read_only);
}

/// A darkmux-managed Node.js release folder under `home`, shaped like
/// the official archive: `bin/node`, and npm a link into `lib`.
fn a_managed_node(home: &Path) -> Managed {
    let root = home.join(".darkmux/toolchains/node-v26.13.1-darwin-arm64");
    program(&root.join("bin/node"), "\x7fELF");
    program(&root.join("lib/node_modules/npm/bin/npm-cli.js"), "#!/usr/bin/env node\n");
    link(Path::new("../lib/node_modules/npm/bin/npm-cli.js"), &root.join("bin/npm"));
    Managed { bin: root.join("bin"), root, env: vec![("NODE_OPTIONS".into(), "--require \"/p.cjs\"".into())] }
}

/// (#3561) A managed toolchain is searched before the host's PATH: npm and
/// the node its script asks for are found in it, its whole release folder is
/// the one grant (npm's own files under `lib` included), its `bin` is first
/// on the sandbox's PATH, and the host's nvm, shadowed, is granted nothing.
#[test]
fn a_managed_toolchain_is_found_first_and_granted_whole() {
    let host = a_fake_host();
    let managed = a_managed_node(&host.home);
    let system = host.system.clone();
    let reads = move |p: &Path| p.starts_with(&system);
    let path = host.path_var();
    let ask = |managed: &[Managed]| Ask {
        commands: vec!["npm run dev"],
        workdir: None,
        path_var: &path,
        home: Some(&host.home),
        reads: &reads,
        managed: managed.to_vec(),
        placed: "x".into(),
        runs_with: "y",
    };
    let got = resolve(&ask(std::slice::from_ref(&managed))).unwrap();
    assert_eq!(got.read_only, vec![managed.root.clone()], "{:?}", got.read_only);
    assert_eq!(got.env, managed.env, "the managed toolchain's environment rides with it");
    let first = got.path.as_deref().unwrap().split(':').next().unwrap().to_string();
    assert_eq!(first, managed.bin.display().to_string(), "{:?}", got.path);
    assert!(got.identity.contains(&format!("npm={}", managed.root.join("lib/node_modules/npm/bin/npm-cli.js").display())), "{}", got.identity);
    // Without it, the host's nvm answers as before.
    let host_only = resolve(&ask(&[])).unwrap();
    assert_eq!(host_only.read_only, vec![host.nvm.clone()]);
}

/// (#3561) A sandbox with no recipe to resolve (a dispatch, a contained
/// command) gets the managed toolchain alone over its backend's own PATH:
/// its `bin` first, its folder the one read-only grant.
#[test]
fn a_managed_toolchain_goes_over_a_backends_own_path() {
    let host = a_fake_host();
    let managed = a_managed_node(&host.home);
    let got = with_managed(std::slice::from_ref(&managed), "/usr/bin:/bin");
    assert_eq!(got.path.as_deref(), Some(format!("{}:/usr/bin:/bin", managed.bin.display()).as_str()));
    assert_eq!(got.read_only, vec![managed.root.clone()]);
    assert_eq!(got.identity, format!("managed={}", managed.root.display()));
    assert_eq!(got.env, managed.env);
    assert_eq!(with_managed(&[], "/usr/bin:/bin"), Toolchain { path: Some("/usr/bin:/bin".into()), ..Toolchain::default() });
}

/// (#3561) A managed toolchain's environment is appended to what the
/// policy already carries, never replacing it; set where it carries none.
#[test]
fn a_managed_toolchains_env_is_appended_never_replacing() {
    let t = Toolchain { env: vec![("NODE_OPTIONS".into(), "--require \"/p.cjs\"".into())], ..Toolchain::default() };
    let mut env = vec![("NODE_OPTIONS".to_string(), "--max-old-space-size=4096".to_string()), ("A".into(), "1".into())];
    t.append_env(&mut env);
    assert_eq!(env[0].1, "--max-old-space-size=4096 --require \"/p.cjs\"");
    let mut none = vec![("A".to_string(), "1".to_string())];
    t.append_env(&mut none);
    assert_eq!(none.last(), Some(&("NODE_OPTIONS".to_string(), "--require \"/p.cjs\"".to_string())));
    let mut twice = vec![("NODE_OPTIONS".to_string(), "-a".to_string()), ("NODE_OPTIONS".to_string(), "".to_string())];
    t.append_env(&mut twice);
    assert_eq!(twice, vec![("NODE_OPTIONS".to_string(), "-a".to_string()), ("NODE_OPTIONS".to_string(), "--require \"/p.cjs\"".to_string())], "the last setting is the one that counts");
}

/// (#3112 review, BLOCKER B) Nothing a workspace file names is ever
/// granted: a script whose `#!` is a workspace path linked to a host secret
/// (an SSH key, gh's `hosts.yml`, not even executable) grants neither the
/// secret nor its folder. Only the interpreter's NAME counts, and only a
/// known runtime's, found on PATH like a recipe program: `<ws>/evil/node`
/// resolves to the PATH's node; `<ws>/evil/hosts.yml` grants nothing.
#[test]
fn a_workspace_shebang_never_grants_the_path_it_names() {
    let host = a_fake_host();
    std::fs::create_dir_all(host.home.join(".ssh")).unwrap();
    std::fs::write(host.home.join(".ssh/id_ed25519"), "secret").unwrap();
    std::fs::create_dir_all(host.home.join(".config/gh")).unwrap();
    std::fs::write(host.home.join(".config/gh/hosts.yml"), "token: secret").unwrap();
    let ws = tempfile::TempDir::new().unwrap();
    let ws = ws.path().canonicalize().unwrap();
    link(&host.home.join(".ssh/id_ed25519"), &ws.join("evil/node"));
    link(&host.home.join(".config/gh/hosts.yml"), &ws.join("evil/hosts.yml"));
    program(&ws.join("a.sh"), &format!("#!{}\n", ws.join("evil/node").display()));
    program(&ws.join("b.sh"), &format!("#!{}\n", ws.join("evil/hosts.yml").display()));
    program(&ws.join("c.sh"), &format!("#!/usr/bin/env {}\n", ws.join("evil/node").display()));
    let secrets = [host.home.join(".ssh/id_ed25519"), host.home.join(".config/gh/hosts.yml")];
    for script in ["./a.sh", "./b.sh", "./c.sh"] {
        let got = host.resolve(&[script], Some(&ws)).unwrap();
        for secret in &secrets {
            assert!(!got.read_only.iter().any(|g| secret.starts_with(g)), "{script} granted {}: {:?}", secret.display(), got.read_only);
        }
        assert!(got.read_only.iter().all(|g| g == &host.nvm), "{script} granted beyond the PATH's node: {:?}", got.read_only);
    }
    assert_eq!(host.resolve(&["./a.sh"], Some(&ws)).unwrap().read_only, vec![host.nvm.clone()], "`<ws>/evil/node` is the PATH's node");
    assert_eq!(host.resolve(&["./c.sh"], Some(&ws)).unwrap().read_only, vec![host.nvm.clone()], "`env <ws>/evil/node` is the PATH's node");
    assert!(host.resolve(&["./b.sh"], Some(&ws)).unwrap().read_only.is_empty(), "hosts.yml is no runtime");
}

/// (#3112 review, M07) A home that is itself shaped like a version folder
/// (`/…/versions/<name>`) is never granted as one: `~/bin`'s program gets
/// `~/bin`.
#[test]
fn a_version_shaped_home_is_never_granted_as_a_version_folder() {
    let dir = tempfile::TempDir::new().unwrap();
    let home = dir.path().canonicalize().unwrap().join("versions/me");
    program(&home.join("bin/tool"), "\x7fELF");
    assert_eq!(grant_for(&home.join("bin/tool"), Some(&home)), Ok(home.join("bin")));
}

/// (#3112 review) A recipe word that is an absolute path to a file that is
/// not a program (gh's `hosts.yml`) grants nothing.
#[test]
fn an_absolute_recipe_word_that_is_no_program_grants_nothing() {
    let host = a_fake_host();
    std::fs::create_dir_all(host.home.join(".config/gh")).unwrap();
    std::fs::write(host.home.join(".config/gh/hosts.yml"), "token: secret").unwrap();
    let word = host.home.join(".config/gh/hosts.yml").display().to_string();
    let got = host.resolve(&[word.as_str()], None).unwrap();
    assert!(got.read_only.is_empty(), "{:?}", got.read_only);
}

/// (#3112 review) A workspace script that is a FIFO (or any file that is
/// not a regular one) is never opened to read its `#!`: resolving returns at
/// once, granting nothing for it, where opening it would wait forever for a
/// writer.
#[test]
fn a_workspace_script_that_is_a_fifo_never_hangs_the_resolution() {
    let host = a_fake_host();
    let ws = tempfile::TempDir::new().unwrap();
    let ws = ws.path().canonicalize().unwrap();
    std::fs::create_dir_all(ws.join("node_modules/.bin")).unwrap();
    let fifo = ws.join("node_modules/.bin/vite");
    let made = std::process::Command::new("/usr/bin/mkfifo").arg(&fifo).status().unwrap();
    assert!(made.success());
    let (tx, rx) = std::sync::mpsc::channel();
    let path = host.path_var();
    let (home, system, ws2) = (host.home.clone(), host.system.clone(), ws.clone());
    std::thread::spawn(move || {
        let reads = move |p: &Path| p.starts_with(&system);
        let got = resolve(&Ask { commands: vec!["./node_modules/.bin/vite"], workdir: Some(&ws2), path_var: &path, home: Some(&home), reads: &reads, managed: Vec::new(), placed: "x".into(), runs_with: "y" });
        let _ = tx.send(got);
    });
    let got = rx.recv_timeout(std::time::Duration::from_secs(3));
    if got.is_err() {
        // Unblock the stuck open so the thread ends with the test.
        let _ = std::fs::OpenOptions::new().write(true).open(&fifo).map(|mut f| std::io::Write::write_all(&mut f, b"#!/bin/sh\n"));
    }
    let got = got.expect("resolving a FIFO script hung").unwrap();
    assert!(got.read_only.is_empty(), "{:?}", got.read_only);
}

/// A recipe program named by its absolute path is granted where it really
/// is (nvm's node: its version folder), as one found on PATH would be,
/// and is part of the toolchain's identity.
#[test]
fn an_absolute_recipe_program_is_granted_where_it_is() {
    let host = a_fake_host();
    let node = host.nvm.join("bin/node").display().to_string();
    let got = host.resolve(&[format!("{node} server.js").as_str()], None).unwrap();
    assert_eq!(got.read_only, vec![host.nvm.clone()]);
    assert!(got.identity.contains(&format!("node={node}")), "{}", got.identity);
}

/// A relative recipe word that is no file in the workspace (an argument,
/// or a script not written yet) grants nothing and refuses nothing, with a
/// workspace or without one.
#[test]
fn a_relative_recipe_word_with_no_file_behind_it_grants_nothing() {
    let host = a_fake_host();
    let ws = tempfile::TempDir::new().unwrap();
    for workdir in [Some(ws.path()), None] {
        let got = host.resolve(&["./build.sh --watch"], workdir).unwrap();
        assert!(got.read_only.is_empty(), "{:?}", got.read_only);
        assert!(got.identity.is_empty(), "{}", got.identity);
    }
}
