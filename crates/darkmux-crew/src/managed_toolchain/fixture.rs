//! (#3561) A local stand-in for a runtime's download host, for test
//! builds: every pinned version, for this host's platform, as a small
//! archive shaped like the official one (`<top>/bin/<runtime>`, and for
//! Node `bin/npm` a link into `lib/node_modules/npm`), pinned by its own
//! SHA-256 and served from a `file://` folder. A sandbox start in a test
//! installs it through the same download, hash check and atomic install
//! production uses, and reaches no network. Its program is a shell script
//! printing `v<version>` and then its `NODE_OPTIONS`.

use super::{Pin, Runtime, Source};
use std::path::Path;
use std::sync::Mutex;

/// Write a release folder `top` under `into` for `runtime` at `version`:
/// its program, and npm as Node ships it (a link into `lib`).
pub fn release_tree(into: &Path, top: &str, runtime: &str, version: &str) {
    use std::os::unix::fs::PermissionsExt;
    let root = into.join(top);
    let exe = |path: &Path, text: &str| {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    exe(
        &root.join("bin").join(runtime),
        &format!("#!/bin/sh\necho v{version}\necho \"$NODE_OPTIONS\"\n"),
    );
    exe(
        &root.join("lib/node_modules/npm/bin/npm-cli.js"),
        "#!/bin/sh\necho 11.0.0\n",
    );
    std::os::unix::fs::symlink(
        "../lib/node_modules/npm/bin/npm-cli.js",
        root.join("bin/npm"),
    )
    .unwrap();
}

/// The release folder `top` under `staging` packed as `archive`, the way
/// the official archives are (the folder at the archive's top).
pub fn pack(staging: &Path, top: &str, archive: &Path) {
    std::fs::create_dir_all(archive.parent().unwrap()).unwrap();
    // No AppleDouble `._` entries for extended attributes (bsdtar on a Mac).
    let ok = std::process::Command::new("tar")
        .env("COPYFILE_DISABLE", "1")
        .arg("-czf")
        .arg(archive)
        .arg("-C")
        .arg(staging)
        .arg(top)
        .status()
        .unwrap();
    assert!(ok.success(), "tar -czf {}", archive.display());
}

/// `runtime`'s fixture for this host: built once per process under its
/// isolated scratch folder.
pub fn source(runtime: &Runtime) -> Source {
    static BUILT: Mutex<Vec<(&'static str, Source)>> = Mutex::new(Vec::new());
    let mut built = BUILT.lock().unwrap();
    if let Some((_, s)) = built.iter().find(|(n, _)| *n == runtime.name) {
        return s.clone();
    }
    let dir = darkmux_types::paths::test_isolated_dir("toolchain-dist").join(runtime.name);
    let s = build(
        runtime,
        &dir,
        darkmux_platform::current().node_release_platform(),
    );
    built.push((runtime.name, s.clone()));
    s
}

/// Every pin of `runtime` for `platform`, packed under `root` as
/// `<root>/<version>/<archive>`, and the source serving them, each at its
/// `file://` URL with its own archive's hash pinned.
pub fn build(runtime: &Runtime, root: &Path, platform: Option<&str>) -> Source {
    let staging = root.join("staging");
    let mut pins = Vec::new();
    for pin in runtime.pins.iter().filter(|p| Some(p.platform) == platform) {
        let archive = root.join(pin.version.to_string()).join(pin.archive());
        if !archive.exists() {
            let _ = std::fs::remove_dir_all(staging.join(pin.top));
            release_tree(&staging, pin.top, pin.runtime, &pin.version.to_string());
            pack(&staging, pin.top, &archive);
        }
        let sha = darkmux_types::sha256::file_hex(&archive).unwrap();
        let url = format!("file://{}", archive.display());
        pins.push(Pin {
            sha256: leak(sha),
            url: leak(url),
            ..*pin
        });
    }
    Source { pins }
}

/// A test build's string that lives as long as the pins it is in.
pub fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}
