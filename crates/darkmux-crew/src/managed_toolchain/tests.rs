//! (#3561) The managed runtimes (Node): which version a project gets, the
//! verified atomic install, its refusals, the cleanup, its extras (the
//! listen preload) and its flow records. Every download is from a local
//! `file://` fixture with its own pinned hashes; no test reaches the
//! network. The preload's behavior is checked against a real `node`.

use super::*;
use darkmux_types::session_id::{RunId, SessionId};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A platform whose pins the fixture builds, whatever this host is.
const PLATFORM: &str = "darwin-arm64";

fn a_dir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().canonicalize().unwrap();
    (dir, root)
}

/// A local download host under `root/dist`, and the toolchains folder to
/// install into.
fn a_source(root: &Path) -> (Source, PathBuf) {
    (
        fixture::build(&NODE, &root.join("dist"), Some(PLATFORM)),
        root.join("toolchains"),
    )
}

fn pin_of(source: &Source, major: u32) -> Pin {
    *source
        .pins
        .iter()
        .find(|p| p.version.major == major)
        .unwrap()
}

fn request(from: Declared, text: &str) -> Request {
    Request {
        from,
        dir: PathBuf::from("/ws"),
        text: text.to_string(),
    }
}

/// What `text` (in `from`) picks among the real pins for one platform.
fn pick(from: Declared, text: &str) -> Result<String, String> {
    let pins = Source::official(&NODE).pins_for(PLATFORM);
    choose(Some(&request(from, text)), &pins).map(|p| p.version.to_string())
}

fn note_word(n: &Note) -> String {
    match n {
        Note::Fetching { pin, .. } => format!("fetching {}", pin.version),
        Note::Ready { pin, .. } => format!("ready {}", pin.version),
        Note::Failed { pin, .. } => format!("failed {}", pin.version),
    }
}

fn install(pin: &Pin, toolchains: &Path, source: &Source) -> Result<Installed, String> {
    ensure(&NODE, pin, toolchains, source, &mut |_| {})
}

/// The entries of `root` whose names start with `.` (staging folders).
fn staging_left(root: &Path) -> Vec<String> {
    std::fs::read_dir(root)
        .map(|r| {
            r.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with('.'))
                .collect()
        })
        .unwrap_or_default()
}

/// (#3561) The pin table: every row keyed by (runtime, version,
/// platform) with its official URL and the archive folder holding `bin`;
/// each LTS line for each of the four platforms; each hash 64 lowercase
/// hex digits. A project with no declaration gets the newest pin of the
/// default line.
#[test]
fn the_pins_cover_every_line_and_platform_and_the_default_is_the_active_lts() {
    let pins = Source::official(&NODE).pins_for(PLATFORM);
    assert_eq!(
        choose(None, &pins).unwrap().version,
        Version::new(26, 13, 1)
    );
    for platform in ["darwin-arm64", "darwin-x64", "linux-x64", "linux-arm64"] {
        let majors: Vec<u32> = Source::official(&NODE)
            .pins_for(platform)
            .iter()
            .map(|p| p.version.major)
            .collect();
        assert_eq!(majors, [26, 24, 22], "{platform}");
    }
    for p in NODE.pins {
        assert!(
            p.sha256.len() == 64
                && p.sha256
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            "{p:?}"
        );
        assert_eq!(p.runtime, "node");
        assert_eq!(
            p.url,
            format!(
                "https://nodejs.org/dist/v{0}/node-v{0}-{1}.tar.gz",
                p.version, p.platform
            )
        );
        assert_eq!(p.top, format!("node-v{}-{}", p.version, p.platform));
    }
    let first = NODE.pins[0];
    assert_eq!(
        (first.archive(), first.dir_name().as_str()),
        (
            "node-v26.13.1-darwin-arm64.tar.gz",
            "node-26.13.1-darwin-arm64"
        )
    );
    assert!(RUNTIMES.iter().any(|r| r.name == "node"));
}

/// (#3561) Every form darkmux reads resolves to the newest pin it allows.
#[test]
fn each_supported_form_resolves_to_the_newest_pin_it_allows() {
    let cases = [
        (Declared::Nvmrc, "22.23.5", "22.23.5"),
        (Declared::Nvmrc, "v24.22.3", "24.22.3"),
        (Declared::Nvmrc, "22", "22.23.5"),
        (Declared::NodeVersion, "v24", "24.22.3"),
        (Declared::Nvmrc, "22.23", "22.23.5"),
        (Declared::Nvmrc, "22.x", "22.23.5"),
        (Declared::Engines, "*", "26.13.1"),
        (Declared::Engines, "", "26.13.1"),
        (Declared::Engines, ">=20", "26.13.1"),
        (Declared::Engines, ">= 20", "26.13.1"),
        (Declared::Engines, ">=20 <25", "24.22.3"),
        (Declared::Engines, ">=20.0.0 <=24", "24.22.3"),
        (Declared::Engines, "<26", "24.22.3"),
        (Declared::Engines, ">24", "26.13.1"),
        (Declared::Engines, ">24.22.2 <26", "24.22.3"),
        (Declared::Engines, ">22.23", "26.13.1"),
        (Declared::Engines, "=22.23.5", "22.23.5"),
        (Declared::Engines, "^22.1.0", "22.23.5"),
        (Declared::Engines, "^24", "24.22.3"),
        (Declared::Engines, "~24.22", "24.22.3"),
        (Declared::Engines, "~22", "22.23.5"),
        (Declared::Engines, "20 - 24", "24.22.3"),
        (Declared::Engines, "20 - 24.22.3", "24.22.3"),
        (Declared::Engines, "22 || 24", "24.22.3"),
        (Declared::Engines, "^18 || ^22", "22.23.5"),
        (Declared::Nvmrc, "lts/*", "26.13.1"),
        (Declared::Nvmrc, "lts/jod", "22.23.5"),
        (Declared::NodeVersion, "LTS/Krypton", "24.22.3"),
    ];
    for (from, text, want) in cases {
        assert_eq!(pick(from, text).as_deref(), Ok(want), "{from:?} `{text}`");
    }
}

/// (#3561) A version no pin satisfies is refused, naming the nearest pin
/// and every pinned version: only a pinned build is ever fetched.
#[test]
fn an_unpinned_version_is_refused_naming_the_nearest_pin() {
    let why = pick(Declared::Nvmrc, "22.5.0").unwrap_err();
    assert!(
        why.contains("`22.5.0`")
            && why.contains("nearest is 22.23.5")
            && why.contains("26.13.1, 24.22.3, 22.23.5"),
        "{why}"
    );
    let why = pick(Declared::Engines, ">=27").unwrap_err();
    assert!(
        why.contains("nearest is 26.13.1") && why.contains("engines.node"),
        "{why}"
    );
    assert!(pick(Declared::Nvmrc, "18")
        .unwrap_err()
        .contains("nearest is 22.23.5"));
    assert!(pick(Declared::Engines, "<22")
        .unwrap_err()
        .contains("nearest is 22.23.5"));
}

/// (#3561) Every form darkmux does not read is refused by name, saying
/// which forms it reads; `lts/<codename>` with no pin lists the ones it has.
#[test]
fn an_unreadable_form_is_refused_by_name() {
    for text in [
        "node",
        "latest",
        "current",
        "stable",
        "system",
        "iojs",
        "22.0.0-rc.1",
        "22.1.0+build",
        "lts",
        "twenty",
        ">=",
        "1.2.3.4",
        "22.x.3",
        ">*",
    ] {
        let why = pick(Declared::Nvmrc, text).unwrap_err();
        assert!(why.contains(&format!("`{text}`")), "{text}: {why}");
        if text != ">*" {
            assert!(
                why.contains("cannot read") && why.contains("lts/<codename>"),
                "{text}: {why}"
            );
        }
    }
    assert!(
        pick(Declared::Engines, "lts/*")
            .unwrap_err()
            .contains("cannot read"),
        "engines.node is semver, not an nvm alias"
    );
    let why = pick(Declared::Nvmrc, "lts/argon").unwrap_err();
    assert!(
        why.contains("`lts/lithium`, `lts/krypton`, `lts/jod`"),
        "{why}"
    );
}

/// (#3561 review) A version part at the largest a part holds is refused as
/// unreadable wherever its bound would step past it: never a panic, and
/// never a wrap-around (`>4294967295` read as `>=0.0.0` picked 26).
#[test]
fn a_bound_past_the_largest_version_part_is_refused_not_wrapped() {
    for text in [
        "4294967295",
        "~4294967295",
        ">4294967295",
        "22.4294967295",
        "^0.0.4294967295",
        "<=4294967295",
        "^4294967295",
        "~22.4294967295",
        "1 - 4294967295",
    ] {
        let why = pick(Declared::Nvmrc, text).unwrap_err();
        assert!(
            why.contains("cannot read") && why.contains(&format!("`{text}`")),
            "{text}: {why}"
        );
    }
    assert!(
        pick(Declared::Engines, ">=4294967295")
            .unwrap_err()
            .contains("no version darkmux pins satisfies"),
        "a bound that needs no step is read"
    );
}

/// (#3561 review) A leading UTF-8 byte order mark (npm strips it from
/// `package.json`) is not part of a declaration, in any of the three files.
#[test]
fn a_byte_order_mark_is_not_part_of_a_declaration() {
    let (_d, root) = a_dir();
    for (file, text, want) in [
        (
            "package.json",
            "\u{feff}{\"engines\": {\"node\": \"^22\"}}",
            "^22",
        ),
        (".nvmrc", "\u{feff}24\n", "24"),
        (".node-version", "\u{feff}v22.23.5\n", "v22.23.5"),
    ] {
        let ws = root.join(file.trim_start_matches('.'));
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join(file), text).unwrap();
        let got = declared_in(&[&ws])
            .unwrap()
            .unwrap_or_else(|| panic!("{file} declared nothing"));
        assert_eq!(got.text, want, "{file}");
        assert!(
            choose(Some(&got), &Source::official(&NODE).pins_for(PLATFORM)).is_ok(),
            "{file}"
        );
    }
}

/// (#3561) Where a project declares its version: `.nvmrc`, then
/// `.node-version`, then `engines.node`, in the first folder that has any;
/// a comment and blank lines skipped; nothing declared is `None`.
#[test]
fn the_first_declaration_wins_in_file_then_folder_order() {
    let (_d, root) = a_dir();
    let (pkg, ws) = (root.join("ws/pkg"), root.join("ws"));
    std::fs::create_dir_all(&pkg).unwrap();
    assert_eq!(declared_in(&[&pkg, &ws]).unwrap(), None);
    std::fs::write(ws.join("package.json"), r#"{"engines": {"node": ">=22"}}"#).unwrap();
    let got = declared_in(&[&pkg, &ws]).unwrap().unwrap();
    assert_eq!(
        (got.from, got.text.as_str(), got.dir.as_path()),
        (Declared::Engines, ">=22", ws.as_path())
    );
    std::fs::write(ws.join(".node-version"), "24\n").unwrap();
    assert_eq!(
        declared_in(&[&pkg, &ws]).unwrap().unwrap().from,
        Declared::NodeVersion
    );
    std::fs::write(
        ws.join(".nvmrc"),
        "# the project's Node\n\n  lts/jod  # comment\n",
    )
    .unwrap();
    let got = declared_in(&[&pkg, &ws]).unwrap().unwrap();
    assert_eq!((got.from, got.text.as_str()), (Declared::Nvmrc, "lts/jod"));
    std::fs::write(
        pkg.join("package.json"),
        r#"{"name": "pkg", "engines": {"node": "22"}}"#,
    )
    .unwrap();
    let got = declared_in(&[&pkg, &ws]).unwrap().unwrap();
    assert_eq!(
        (got.from, got.dir.as_path()),
        (Declared::Engines, pkg.as_path()),
        "the nearer folder wins"
    );
}

/// (#3561) A declaration is read only from a regular file of the folder
/// itself: a link is refused, never followed (the workspace is written by a
/// run); a non-string `engines.node` is refused; a `package.json` that does
/// not parse declares nothing.
#[test]
fn a_declaration_is_read_only_from_a_regular_file() {
    let (_d, root) = a_dir();
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(root.join("secret"), "22\n").unwrap();
    std::os::unix::fs::symlink(root.join("secret"), ws.join(".nvmrc")).unwrap();
    let why = declared_in(&[&ws]).unwrap_err();
    assert!(
        why.contains(".nvmrc") && why.contains("never reads through a link"),
        "{why}"
    );
    std::fs::remove_file(ws.join(".nvmrc")).unwrap();
    std::fs::create_dir(ws.join(".node-version")).unwrap();
    assert!(declared_in(&[&ws])
        .unwrap_err()
        .contains("not a regular file"));
    std::fs::remove_dir(ws.join(".node-version")).unwrap();
    std::fs::write(ws.join(".nvmrc"), "2".repeat(5000)).unwrap();
    assert!(declared_in(&[&ws]).unwrap_err().contains("larger than"));
    std::fs::remove_file(ws.join(".nvmrc")).unwrap();
    std::fs::write(ws.join("package.json"), r#"{"engines": {"node": 22}}"#).unwrap();
    assert!(declared_in(&[&ws]).unwrap_err().contains("not a string"));
    std::fs::write(ws.join("package.json"), "{ not json").unwrap();
    assert_eq!(declared_in(&[&ws]).unwrap(), None);
}

/// (#3561) The first use downloads, checks the pinned hash, installs
/// atomically at `<runtime>-<version>-<platform>` with the listen preload
/// read-only in it and says so; the install runs; a second use is the
/// verified install with no fetch.
#[test]
fn the_first_use_installs_a_verified_node_and_later_uses_fetch_nothing() {
    let (_d, root) = a_dir();
    let (source, toolchains) = a_source(&root);
    let pin = pin_of(&source, 26);
    let mut notes = Vec::new();
    let got = ensure(&NODE, &pin, &toolchains, &source, &mut |n| {
        notes.push(note_word(n))
    })
    .unwrap();
    assert_eq!(notes, ["fetching 26.13.1", "ready 26.13.1"]);
    assert_eq!(got.dir, toolchains.join("node-26.13.1-darwin-arm64"));
    let preload = got.dir.join("darkmux/listen-loopback.cjs");
    assert_eq!(
        got.env,
        vec![(
            "NODE_OPTIONS".to_string(),
            format!("--require \"{}\"", preload.display())
        )]
    );
    assert_eq!(
        got.managed(),
        darkmux_sandbox::toolchain::Managed {
            root: got.dir.clone(),
            bin: got.dir.join("bin"),
            env: got.env.clone()
        }
    );
    assert_eq!(
        std::fs::read_to_string(&preload).unwrap(),
        node::LISTEN_PRELOAD
    );
    assert!(
        std::fs::metadata(&preload)
            .unwrap()
            .permissions()
            .readonly(),
        "the preload is read-only"
    );
    let out = std::process::Command::new(got.bin().join("node"))
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("v26.13.1\n"));
    let marker = std::fs::read_to_string(got.dir.join(MARKER)).unwrap();
    assert_eq!(
        marker,
        format!("node 26.13.1 darwin-arm64 sha256 {}\n", pin.sha256)
    );
    assert!(
        std::fs::symlink_metadata(got.bin().join("npm"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "npm is a link, as Node ships it"
    );
    assert_eq!(staging_left(&toolchains), Vec::<String>::new());
    notes.clear();
    assert_eq!(
        ensure(&NODE, &pin, &toolchains, &source, &mut |n| notes
            .push(note_word(n)))
        .unwrap(),
        got
    );
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(installed(&NODE, &pin, &toolchains), Ok(Some(got)));
}

/// (#3561) A preload an older darkmux wrote (or one removed) is put back as
/// this darkmux's, in place and read-only, with no new download.
#[test]
fn a_stale_preload_is_replaced_without_a_download() {
    let (_d, root) = a_dir();
    let (source, toolchains) = a_source(&root);
    let pin = pin_of(&source, 24);
    let got = install(&pin, &toolchains, &source).unwrap();
    let preload = got.dir.join("darkmux/listen-loopback.cjs");
    for older in [Some("// an older darkmux's preload\n"), None] {
        std::fs::remove_file(&preload).unwrap();
        if let Some(text) = older {
            std::fs::write(&preload, text).unwrap();
        }
        let mut fetched = false;
        ensure(&NODE, &pin, &toolchains, &source, &mut |_| fetched = true).unwrap();
        assert!(!fetched, "a stale preload refetched the archive");
        assert_eq!(
            std::fs::read_to_string(&preload).unwrap(),
            node::LISTEN_PRELOAD
        );
        assert!(std::fs::metadata(&preload)
            .unwrap()
            .permissions()
            .readonly());
    }
}

/// (#3561) An archive that does not hash to its pin installs nothing and
/// says why: never an unverified binary.
#[test]
fn a_hash_mismatch_installs_nothing() {
    let (_d, root) = a_dir();
    let (mut source, toolchains) = a_source(&root);
    let mut pin = pin_of(&source, 24);
    pin.sha256 = "0000000000000000000000000000000000000000000000000000000000000000";
    source.pins = vec![pin];
    let mut notes = Vec::new();
    let why = ensure(&NODE, &pin, &toolchains, &source, &mut |n| {
        notes.push(note_word(n))
    })
    .unwrap_err();
    assert!(
        why.contains("does not match the SHA-256 darkmux pins for Node 24.22.3")
            && why.contains("Nothing was installed"),
        "{why}"
    );
    assert_eq!(notes, ["fetching 24.22.3", "failed 24.22.3"]);
    assert!(!toolchains.join(pin.dir_name()).exists());
    assert_eq!(staging_left(&toolchains), Vec::<String>::new());
    assert_eq!(installed(&NODE, &pin, &toolchains), Ok(None));
}

/// (#3561) A download that fails installs nothing and names the fix.
#[test]
fn a_failed_download_installs_nothing_and_names_the_fix() {
    let (_d, root) = a_dir();
    let (source, toolchains) = a_source(&root);
    let pin = Pin {
        url: fixture::leak(format!(
            "file://{}",
            root.join("nowhere/x.tar.gz").display()
        )),
        ..pin_of(&source, 22)
    };
    let why = install(&pin, &toolchains, &source).unwrap_err();
    assert!(
        why.contains("could not download") && why.contains("reaches the download's host"),
        "{why}"
    );
    assert!(!toolchains.join(pin.dir_name()).exists());
    assert_eq!(staging_left(&toolchains), Vec::<String>::new());
}

/// (#3561) An archive that hashes to its pin but fails to unpack partway
/// (cut short) never becomes usable: nothing is at the install's path, so
/// no later sandbox finds a half-unpacked runtime.
#[test]
fn a_partial_unpack_never_becomes_an_install() {
    let (_d, root) = a_dir();
    let (source, toolchains) = a_source(&root);
    let pin = pin_of(&source, 26);
    let archive = root.join("cut").join(pin.archive());
    let staging = root.join("big");
    fixture::release_tree(&staging, pin.top, "node", "26.13.1");
    for i in 0..40 {
        let noise: String = (0..20_000)
            .map(|j| char::from(b'a' + ((i * 7 + j * 13) % 26) as u8))
            .collect();
        std::fs::write(staging.join(pin.top).join(format!("lib/f{i}.txt")), noise).unwrap();
    }
    fixture::pack(&staging, pin.top, &archive);
    let bytes = std::fs::read(&archive).unwrap();
    std::fs::write(&archive, &bytes[..bytes.len() * 2 / 3]).unwrap();
    let sha = fixture::leak(darkmux_types::sha256::file_hex(&archive).unwrap());
    let cut = Pin {
        sha256: sha,
        url: fixture::leak(format!("file://{}", archive.display())),
        ..pin
    };
    let source = Source { pins: vec![cut] };
    let why = install(&cut, &toolchains, &source).unwrap_err();
    assert!(
        why.contains("unpacking") && why.contains("Nothing was installed"),
        "{why}"
    );
    assert!(
        !toolchains.join(cut.dir_name()).exists(),
        "a partial unpack is at the install's path"
    );
    let left: Vec<_> = std::fs::read_dir(&toolchains)
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert!(
        left.is_empty(),
        "a failed install left {left:?} in the toolchains folder"
    );
}

/// (#3561) A folder at the install's path that darkmux did not verify is
/// neither used nor deleted: the start is refused, naming how to clear it.
#[test]
fn an_unverified_folder_at_the_install_path_is_refused_and_kept() {
    let (_d, root) = a_dir();
    let (source, toolchains) = a_source(&root);
    let pin = pin_of(&source, 26);
    let theirs = toolchains.join(pin.dir_name());
    fixture::release_tree(&toolchains, &pin.dir_name(), "node", "6.6.6");
    let why = install(&pin, &toolchains, &source).unwrap_err();
    assert!(
        why.contains("not a Node install darkmux verified")
            && why.contains(&format!("rm -rf {}", theirs.display())),
        "{why}"
    );
    assert!(
        theirs.join("bin/node").is_file(),
        "darkmux deleted a folder it did not make"
    );
    std::fs::write(
        theirs.join(MARKER),
        "node 26.13.1 darwin-arm64 sha256 not-the-pin\n",
    )
    .unwrap();
    assert!(
        install(&pin, &toolchains, &source).is_err(),
        "a marker for another hash is not this build's"
    );
}

/// (#3561) Two first uses at once each stage their own copy; both get the
/// one verified install and nothing is left half done.
#[test]
fn concurrent_first_uses_share_one_verified_install() {
    let (_d, root) = a_dir();
    let (source, toolchains) = a_source(&root);
    let pin = pin_of(&source, 26);
    let got: Vec<Result<Installed, String>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..4)
            .map(|_| s.spawn(|| install(&pin, &toolchains, &source)))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for g in &got {
        assert_eq!(g.as_ref().unwrap().dir, toolchains.join(pin.dir_name()));
    }
    assert!(installed(&NODE, &pin, &toolchains).unwrap().is_some());
    assert_eq!(staging_left(&toolchains), Vec::<String>::new());
}

/// (#3561) A staged install that loses the race to the final path uses the
/// winner's verified folder; one that loses to an unverified folder is
/// refused.
#[test]
fn a_lost_rename_uses_the_winners_install_only_when_verified() {
    let (_d, root) = a_dir();
    let (source, toolchains) = a_source(&root);
    let pin = pin_of(&source, 26);
    let winner = install(&pin, &toolchains, &source).unwrap();
    let staged = root.join("staged");
    fixture::release_tree(&staged, pin.top, "node", "26.13.1");
    assert_eq!(
        install::land(&pin, &staged.join(pin.top), &winner.dir),
        Ok(())
    );
    let other = root.join("other");
    fixture::release_tree(&other, "x", "node", "1.0.0");
    let why = install::land(&pin, &staged.join(pin.top), &other.join("x")).unwrap_err();
    assert!(why.contains("installing node 26.13.1"), "{why}");
}

/// The time `path` was last modified.
fn mtime(path: &Path) -> std::time::SystemTime {
    std::fs::metadata(path).unwrap().modified().unwrap()
}

/// Set `path`'s modified time to `at`.
fn set_mtime(path: &Path, at: std::time::SystemTime) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(at)
        .unwrap();
}

/// (#3561) Cleanup removes only what darkmux installed, no longer pins
/// and nobody used for [`UNUSED_FOR`], and staging folders a crashed
/// install left a day ago; a pinned install, an unpinned one used within
/// the week, a folder without darkmux's marker, another runtime's, a link,
/// a file and a fresh staging folder stay. Ages are measured from each
/// folder's own stamp, never a fixed time.
#[test]
fn cleanup_removes_only_unused_darkmux_installs_no_longer_pinned() {
    let (_d, root) = a_dir();
    let (source, toolchains) = a_source(&root);
    let kept = install(&pin_of(&source, 26), &toolchains, &source).unwrap();
    let make = |name: &str, marker: Option<&str>| {
        let dir = toolchains.join(name);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        if let Some(m) = marker {
            std::fs::write(dir.join(MARKER), m).unwrap();
            std::fs::write(dir.join(USED), "").unwrap();
        }
        dir
    };
    let old = make(
        "node-20.20.2-darwin-arm64",
        Some("node 20.20.2 darwin-arm64 sha256 abc\n"),
    );
    let foreign = make("node-18.0.0-darwin-arm64", None);
    let other_tool = make(
        "deno-2.0.0-darwin-arm64",
        Some("deno 2.0.0 darwin-arm64 sha256 abc\n"),
    );
    let outside = make("../outside-node", Some("node 1.0.0 x sha256 y\n"));
    std::os::unix::fs::symlink(&outside, toolchains.join("node-1.0.0-x")).unwrap();
    std::fs::write(toolchains.join("node-9.0.0-x"), "a file").unwrap();
    let crashed = make(".node-26.13.1-darwin-arm64.1-a-0.partial", None);
    let (made, used) = (mtime(&crashed), mtime(&old.join(USED)));
    assert_eq!(
        remove_unpinned(&toolchains, &source.pins, made + Duration::from_secs(60)),
        Vec::<PathBuf>::new()
    );
    assert_eq!(
        remove_unpinned(
            &toolchains,
            &source.pins,
            made + STALE_PARTIAL + Duration::from_secs(1)
        ),
        vec![crashed.clone()]
    );
    assert!(
        old.exists(),
        "an unpinned install used within the week was removed"
    );
    assert_eq!(
        remove_unpinned(
            &toolchains,
            &source.pins,
            used + UNUSED_FOR + Duration::from_secs(1)
        ),
        vec![old.clone()]
    );
    for stays in [
        &kept.dir,
        &foreign,
        &other_tool,
        &outside,
        &toolchains.join("node-1.0.0-x"),
        &toolchains.join("node-9.0.0-x"),
    ] {
        assert!(
            std::fs::symlink_metadata(stays).is_ok(),
            "{} was removed",
            stays.display()
        );
    }
    assert!(
        outside.join(MARKER).exists(),
        "cleanup followed a link out of the toolchains folder"
    );
}

/// (#3561 review) A new install removes an unpinned build unused for a
/// week, and keeps one a still-running older darkmux used today (its
/// sandboxes run on it).
#[test]
fn a_new_install_removes_only_unpinned_builds_unused_for_a_week() {
    let (_d, root) = a_dir();
    let (source, toolchains) = a_source(&root);
    let unpinned = |name: &str| {
        let dir = toolchains.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(MARKER), "node 20.20.2 darwin-arm64 sha256 abc\n").unwrap();
        std::fs::write(dir.join(USED), "").unwrap();
        dir
    };
    let (stale, live) = (
        unpinned("node-20.20.2-darwin-arm64"),
        unpinned("node-20.19.0-darwin-arm64"),
    );
    set_mtime(
        &stale.join(USED),
        mtime(&stale.join(USED)) - UNUSED_FOR - Duration::from_secs(60),
    );
    install(&pin_of(&source, 24), &toolchains, &source).unwrap();
    assert!(
        !stale.exists(),
        "an unpinned build unused for a week stayed"
    );
    assert!(live.exists(), "an unpinned build in use today was removed");
}

/// (#3561 review) Using an install stamps it, at most once a day: a stamp
/// older than [`USE_STAMP_EVERY`] is renewed, a fresher one is left as it
/// is (no write on every sandbox start).
#[test]
fn using_an_install_stamps_it_at_most_daily() {
    let (_d, root) = a_dir();
    let (source, toolchains) = a_source(&root);
    let pin = pin_of(&source, 26);
    let got = install(&pin, &toolchains, &source).unwrap();
    let stamp = got.dir.join(USED);
    let fresh = mtime(&stamp);
    let recent = fresh - Duration::from_secs(3600);
    set_mtime(&stamp, recent);
    installed(&NODE, &pin, &toolchains).unwrap().unwrap();
    assert_eq!(
        mtime(&stamp),
        recent,
        "a stamp under a day old was rewritten"
    );
    let old = fresh - USE_STAMP_EVERY - Duration::from_secs(60);
    set_mtime(&stamp, old);
    installed(&NODE, &pin, &toolchains).unwrap().unwrap();
    assert!(
        mtime(&stamp) > old + USE_STAMP_EVERY,
        "a day-old stamp was not renewed"
    );
    std::fs::remove_file(&stamp).unwrap();
    installed(&NODE, &pin, &toolchains).unwrap().unwrap();
    assert!(stamp.exists(), "a missing stamp was not written");
}

/// (#3561) `provide`: the project's declaration picks the pin, which is
/// installed; a platform with no pins is refused, as is a declaration
/// darkmux will not read, before anything is fetched.
#[test]
fn provide_installs_what_the_project_declares() {
    let (_d, root) = a_dir();
    let (source, toolchains) = a_source(&root);
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join(".nvmrc"), "22\n").unwrap();
    let got = provide(
        &NODE,
        Some(PLATFORM),
        &toolchains,
        &source,
        &[&ws],
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(got.pin.version, Version::new(22, 23, 5));
    let why = provide(&NODE, None, &toolchains, &source, &[&ws], &mut |_| {}).unwrap_err();
    assert!(why.contains("pins no Node build for this machine"), "{why}");
    assert!(provide(
        &NODE,
        Some("linux-riscv64"),
        &toolchains,
        &source,
        &[&ws],
        &mut |_| {}
    )
    .is_err());
    std::fs::write(ws.join(".nvmrc"), "latest\n").unwrap();
    let mut fetched = false;
    assert!(provide(
        &NODE,
        Some(PLATFORM),
        &toolchains,
        &source,
        &[&ws],
        &mut |_| fetched = true
    )
    .is_err());
    assert!(!fetched, "an unreadable declaration fetched something");
}

/// (#3561) A fetch's flow records: `toolchain.fetch`, `.ready` and
/// `.error` (at error level, with the reason), on the run's session when
/// the sandbox has one and none otherwise; never an execution's record.
#[test]
fn a_fetch_writes_one_record_per_step() {
    let pin = NODE.pins[0];
    let session = SessionId::adhoc(RunId::standalone("n3561-test").unwrap(), "coder", "1");
    let fetching = record(
        &Note::Fetching {
            runtime: &NODE,
            pin: &pin,
        },
        ToolchainUse::Dispatch,
        Some(&session),
    );
    assert_eq!(fetching.action, darkmux_flow::FlowAction::ToolchainFetch);
    assert_eq!(fetching.session_id, Some(session.wire()));
    assert_eq!(fetching.execution_id, None);
    let p = fetching.payload_json();
    assert_eq!(
        (
            p["tool"].as_str(),
            p["version"].as_str(),
            p["platform"].as_str()
        ),
        (Some("node"), Some("26.13.1"), Some("darwin-arm64"))
    );
    assert_eq!(
        (p["needed_by"].as_str(), p["url"].as_str()),
        (Some("dispatch"), Some(pin.url))
    );
    let ready = record(
        &Note::Ready {
            runtime: &NODE,
            pin: &pin,
            elapsed: Duration::from_millis(1500),
        },
        ToolchainUse::Preview,
        None,
    );
    assert_eq!(
        (ready.action.clone(), ready.session_id.clone()),
        (darkmux_flow::FlowAction::ToolchainReady, None)
    );
    assert_eq!(ready.payload_json()["elapsed_ms"], 1500);
    let failed = record(
        &Note::Failed {
            runtime: &NODE,
            pin: &pin,
            elapsed: Duration::ZERO,
            reason: "hash mismatch",
        },
        ToolchainUse::Command,
        None,
    );
    assert_eq!(failed.action, darkmux_flow::FlowAction::ToolchainError);
    assert!(matches!(failed.level, darkmux_flow::Level::Error));
    assert_eq!(failed.payload_json()["error"], "hash mismatch");
}

/// (#3561) `darkmux doctor`'s reading: fetched on first use until it is
/// installed, then the verified install; nothing for a machine with no pin.
#[test]
fn the_status_is_will_fetch_until_installed() {
    let (_d, root) = a_dir();
    let (source, toolchains) = a_source(&root);
    let pin = pin_of(&source, 26);
    assert_eq!(
        status_in(&NODE, Some(PLATFORM), &toolchains, &source),
        Status::WillFetch(pin)
    );
    let got = install(&pin, &toolchains, &source).unwrap();
    assert_eq!(
        status_in(&NODE, Some(PLATFORM), &toolchains, &source),
        Status::Installed(got)
    );
    assert_eq!(
        status_in(&NODE, None, &toolchains, &source),
        Status::Unsupported
    );
}

/// A real `node` on this host's PATH (outside any sandbox), for the
/// preload's behavior: its semantics are Node's, not darkmux's.
fn a_real_node() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join("node"))
        .find(|p| {
            p.is_file()
                && std::process::Command::new(p)
                    .arg("-e")
                    .arg("process.exit(0)")
                    .status()
                    .is_ok_and(|s| s.success())
        })
}

/// The preload's checks, as a CommonJS script: each listen shape, the
/// address it got, and whether a client reached it.
const LISTEN_CHECKS: &str = r#"
const http = require('http');
const fs = require('fs');
const path = require('path');
const out = {};
function on(server, ...args) {
  return new Promise((ok, no) => { server.once('error', no); server.once('listening', () => ok(server.address())); server.listen(...args); });
}
function get(port, host) {
  return new Promise((ok) => http.get({ host, port, path: '/' }, (r) => { r.resume(); ok(r.statusCode); }).on('error', (e) => ok(e.code)));
}
const handler = (q, r) => r.end('ok');
// Express's own `app.listen`: `http.createServer(this).listen(...arguments)`.
function express() {
  const app = (q, r) => handler(q, r);
  app.listen = function () { const s = http.createServer(this); return s.listen.apply(s, arguments); };
  return app;
}
(async () => {
  const servers = [];
  const add = (s) => (servers.push(s), s);
  let s = add(http.createServer(handler));
  out.port0 = (await on(s, 0)).address;
  out.port0_reached = await get(s.address().port, '127.0.0.1');
  try { out.fetch_localhost = await (await fetch(`http://localhost:${s.address().port}/`)).text(); } catch (e) { out.fetch_localhost = 'failed ' + (e.cause && e.cause.code); }
  out.none = (await on(add(http.createServer(handler)))).address;
  out.cb = await new Promise((ok) => { const t = add(http.createServer(handler)); t.listen(() => ok(t.address().address)); });
  out.backlog = (await on(add(http.createServer(handler)), 0, 511)).address;
  out.undefined_host = (await on(add(http.createServer(handler)), 0, undefined)).address;
  out.options = (await on(add(http.createServer(handler)), { port: 0 })).address;
  const app = express();
  out.express = await new Promise((ok) => { const t = add(app.listen(0, () => ok(t.address().address))); });
  const st = add(app.listen(0));
  await new Promise((ok) => st.once('listening', ok));
  out.supertest_reached = await get(st.address().port, '127.0.0.1');
  out.explicit_any4 = (await on(add(http.createServer(handler)), 0, '0.0.0.0')).address;
  out.explicit_any6 = (await on(add(http.createServer(handler)), 0, '::')).address;
  out.options_any4 = (await on(add(http.createServer(handler)), { port: 0, host: '0.0.0.0' })).address;
  out.ipv6_only = (await on(add(http.createServer(handler)), { port: 0, ipv6Only: true })).address;
  out.empty_host = (await on(add(http.createServer(handler)), 0, '')).address;
  const sock = path.join(process.argv[2], 'n3561.sock');
  try { fs.unlinkSync(sock); } catch (_) {}
  out.unix = await on(add(http.createServer(handler)), sock);
  for (const t of servers) t.close();
  console.log(JSON.stringify(out));
})();
"#;

/// The ES module check: an `.mjs` project's own server.
const LISTEN_CHECK_ESM: &str = r#"
import http from 'node:http';
const s = http.createServer((q, r) => r.end('ok'));
s.listen(0, () => { console.log(JSON.stringify({ esm: s.address().address })); s.close(); });
"#;

/// Run `script` (`name` in `dir`) under `node` with the preload in
/// `NODE_OPTIONS` (after `--max-old-space-size`, so it is appended) or with
/// none; its JSON line.
fn run_checks(
    node: &Path,
    dir: &Path,
    name: &str,
    script: &str,
    preload: Option<&Path>,
) -> serde_json::Value {
    std::fs::write(dir.join(name), script).unwrap();
    let mut cmd = std::process::Command::new(node);
    cmd.arg(dir.join(name)).arg(dir).env_remove("NODE_OPTIONS");
    if let Some(p) = preload {
        cmd.env(
            "NODE_OPTIONS",
            format!("--max-old-space-size=512 --require \"{}\"", p.display()),
        );
    }
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .last()
            .unwrap()
            .as_bytes(),
    )
    .unwrap()
}

/// (#3561) The listen preload against a real Node: every listen that names
/// no host (a port, none, a callback, a backlog, an `undefined` host, an
/// options object, Express's `app.listen`, supertest's `app.listen(0)`)
/// listens on 127.0.0.1 and is reached there, `fetch('http://localhost')`
/// reaches it too (Node resolves `localhost` to `::1` first and falls back
/// to 127.0.0.1), an ES module project gets it through `NODE_OPTIONS`, and
/// an explicit `0.0.0.0` or `::`, or a unix socket, is left as asked.
/// Without the preload the same server listens on every address, which is
/// what it changes. Skipped, saying so, on a host with no `node` on PATH;
/// `managed_toolchain::live` runs it under each pinned build.
#[test]
fn the_listen_preload_binds_a_hostless_listen_to_the_loopback() {
    let Some(node) = a_real_node() else {
        eprintln!("skipped: no `node` on PATH to run the listen preload under");
        return;
    };
    check_listen_preload(&node);
}

/// [`the_listen_preload_binds_a_hostless_listen_to_the_loopback`] under
/// `node`.
pub(super) fn check_listen_preload(node: &Path) {
    let (_d, root) = a_dir();
    let preload = root.join("listen-loopback.cjs");
    std::fs::write(&preload, node::LISTEN_PRELOAD).unwrap();
    let with = run_checks(node, &root, "checks.cjs", LISTEN_CHECKS, Some(&preload));
    for key in [
        "port0",
        "none",
        "cb",
        "backlog",
        "undefined_host",
        "options",
        "express",
        "ipv6_only",
    ] {
        assert_eq!(
            with[key],
            "127.0.0.1",
            "{key} under {}: {with}",
            node.display()
        );
    }
    assert_eq!(
        (
            with["port0_reached"].as_u64(),
            with["supertest_reached"].as_u64()
        ),
        (Some(200), Some(200)),
        "{with}"
    );
    assert_eq!(with["fetch_localhost"], "ok", "{with}");
    assert_eq!(
        (
            with["explicit_any4"].as_str(),
            with["explicit_any6"].as_str(),
            with["options_any4"].as_str()
        ),
        (Some("0.0.0.0"), Some("::"), Some("0.0.0.0")),
        "{with}"
    );
    assert_eq!(
        with["unix"],
        root.join("n3561.sock").display().to_string(),
        "{with}"
    );
    assert_eq!(
        with["empty_host"], "::",
        "an empty host is left as asked (every address, which the watch stops): {with}"
    );
    let esm = run_checks(node, &root, "server.mjs", LISTEN_CHECK_ESM, Some(&preload));
    assert_eq!(esm["esm"], "127.0.0.1", "{esm}");
    let without = run_checks(node, &root, "checks.cjs", LISTEN_CHECKS, None);
    assert_ne!(
        without["port0"], "127.0.0.1",
        "with no preload Node binds every address: {without}"
    );
}

/// (#3561) Opt-in, against nodejs.org (`--run-ignored only`): every pinned
/// build for this host, downloaded, checked against its pin and installed
/// under a scratch folder (never `~/.darkmux`); the listen preload checked
/// under each; and the default build run in a real sandbox on the host
/// with the toolchain a sandbox gets (its `bin` first on PATH, its folder
/// read-only, the preload in `NODE_OPTIONS`), a server in it listening on
/// 127.0.0.1 and reached through `localhost`.
mod live {
    use super::*;

    #[test]
    #[ignore = "downloads every pinned Node build for this host from nodejs.org (about 150 MB)"]
    fn every_pinned_node_installs_verified_and_runs_the_preload() {
        let platform = darkmux_platform::current()
            .node_release_platform()
            .expect("a pinned platform");
        let (_d, root) = a_dir();
        let source = Source::official(&NODE);
        let mut installs = Vec::new();
        for pin in source.pins_for(platform) {
            let started = std::time::Instant::now();
            let got = ensure(&NODE, &pin, &root, &source, &mut |n| {
                eprintln!("{}", note_word(n))
            })
            .unwrap();
            eprintln!(
                "node {} installed and verified in {:?} at {}",
                pin.version,
                started.elapsed(),
                got.dir.display()
            );
            let v = std::process::Command::new(got.bin().join("node"))
                .arg("--version")
                .output()
                .unwrap();
            assert_eq!(
                String::from_utf8_lossy(&v.stdout).trim(),
                format!("v{}", pin.version)
            );
            check_listen_preload(&got.bin().join("node"));
            installs.push(got);
        }
        if darkmux_sandbox::SeatbeltBackend::available()
            || darkmux_sandbox::LandlockBackend::available().is_ok()
        {
            in_a_sandbox(&installs[0], &root);
        }
    }

    /// `node` from `installed`, in a sandbox on this host built the way a
    /// contained command's is.
    fn in_a_sandbox(installed: &Installed, root: &Path) {
        use darkmux_sandbox::{Access, Limits, Mount, Network, SandboxBackend, SandboxPolicy};
        let backend: &dyn SandboxBackend = if darkmux_sandbox::SeatbeltBackend::available() {
            &darkmux_sandbox::SeatbeltBackend
        } else {
            &darkmux_sandbox::LandlockBackend
        };
        let tree = root.join("n3561-tree");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(
            tree.join("serve.cjs"),
            "const s = require('http').createServer((q, r) => r.end('ok'));\n\
             s.listen(0, async () => { const t = await (await fetch(`http://localhost:${s.address().port}/`)).text();\n\
             console.log(process.version, s.address().address, t); s.close(); });\n",
        )
        .unwrap();
        let toolchain = darkmux_sandbox::toolchain::with_managed(
            &[installed.managed()],
            backend.default_path().unwrap_or_default(),
        );
        let mut env = vec![("PATH".to_string(), toolchain.path.clone().unwrap())];
        toolchain.append_env(&mut env);
        let tree_at = tree.canonicalize().unwrap();
        let mut mounts = vec![Mount {
            host: tree_at.clone(),
            at: "/workspace".into(),
            access: Access::ReadWrite,
        }];
        mounts.extend(crate::contained_command::toolchain_mounts(&toolchain));
        let policy = SandboxPolicy {
            name: format!("n3561-live-{}", std::process::id()),
            image: "host".into(),
            network: Network::Default,
            limits: Limits::default(),
            labels: Vec::new(),
            mounts,
            ports: Vec::new(),
            stdin: false,
            host_alias: None,
            user: None,
            env,
            workdir: Some(tree_at.display().to_string()),
            entrypoint: Some("/bin/sh".into()),
            command: vec![
                "-c".into(),
                "echo \"$NODE_OPTIONS\"; command -v node; node serve.cjs".into(),
            ],
        };
        let (cmd, _) = backend.run(&policy);
        let ran = crate::bounded_command::run_bounded(cmd, Duration::from_secs(60));
        let crate::bounded_command::Bounded::Finished {
            success,
            stdout,
            stderr,
            ..
        } = ran
        else {
            panic!("{ran:?}")
        };
        let out = String::from_utf8_lossy(&stdout);
        eprintln!("in the sandbox:\n{out}{}", String::from_utf8_lossy(&stderr));
        assert!(success, "{out}");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], installed.env[0].1);
        assert_eq!(lines[1], installed.bin().join("node").display().to_string());
        assert_eq!(lines[2], format!("v{} 127.0.0.1 ok", installed.pin.version));
    }
}
