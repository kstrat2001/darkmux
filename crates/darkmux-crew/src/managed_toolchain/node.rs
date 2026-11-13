//! (#3561) Node.js, the one managed runtime darkmux ships: its pins, which
//! version a project asks for ([`super::node_version`]), and its extra, the
//! listen preload (`listen-loopback.cjs`).
//!
//! **Where the hashes come from.** For each version, nodejs.org's
//! `https://nodejs.org/dist/v<version>/SHASUMS256.txt`, whose detached
//! signature (`SHASUMS256.txt.sig`) was checked with `gpgv` against the
//! Node.js release keyring (`gpg-only-active-keys/pubring.kbx` in
//! github.com/nodejs/release-keys) when the rows were written: all three
//! were signed by release key `5BE8A3F6C8A5C01D106C0AD820B1A390B168D356`.
//!
//! **Updating the pins.** The newest release of each LTS line Node still
//! supports (github.com/nodejs/Release's `schedule.json`), for the four
//! platforms below, `.tar.gz` archives (GNU tar on Linux unpacks them with
//! no `xz`). A version dropped from here is removed from
//! `~/.darkmux/toolchains/` by the next install.
//!
//! **The listen preload.** Every sandbox on the host that runs this Node
//! gets `NODE_OPTIONS=--require "<install>/darkmux/listen-loopback.cjs"`,
//! appended to any `NODE_OPTIONS` the sandbox already carries. It makes a
//! listen that names no host (`listen(port)`, `listen(0)`, `listen(cb)`,
//! options with no `host`, `path`, `fd` or `handle`) listen on 127.0.0.1,
//! so a test suite's `app.listen(0)` is not stopped for listening beyond
//! the loopback (#3559). An explicit host (`0.0.0.0` and `::` included), a
//! unix socket, an fd or a handle is left alone, so darkmux's watch still
//! stops a server that asks for every address. `--require` runs before an
//! ES module project's own code too. A project that replaces
//! `NODE_OPTIONS` itself (`NODE_OPTIONS=... node`) loses it, and then the
//! watch is what applies. A Docker sandbox runs its image's Node and never
//! gets it.

use super::pins::{Pin, Version};
use super::{Extra, Runtime};

/// The LTS line a project gets when it declares no version: Node's Active
/// LTS line when these pins were written (26, "Lithium", Active LTS from
/// 2026-10-28).
pub const DEFAULT_LINE: u32 = 26;

/// One row of [`PINS`].
macro_rules! node_pin {
    ($v:literal, ($maj:literal, $min:literal, $pat:literal), $line:literal, $platform:literal, $sha:literal) => {
        Pin {
            runtime: "node",
            version: Version::new($maj, $min, $pat),
            line: $line,
            platform: $platform,
            url: concat!(
                "https://nodejs.org/dist/v",
                $v,
                "/node-v",
                $v,
                "-",
                $platform,
                ".tar.gz"
            ),
            sha256: $sha,
            top: concat!("node-v", $v, "-", $platform),
        }
    };
}

/// Every pinned Node build.
pub const PINS: &[Pin] = &[
    node_pin!(
        "26.13.1",
        (26, 13, 1),
        "lithium",
        "darwin-arm64",
        "aec2514ea1ceb2ba52455e9ec65986f6749a06dd759e5db2a72e6889668c9735"
    ),
    node_pin!(
        "26.13.1",
        (26, 13, 1),
        "lithium",
        "darwin-x64",
        "82bb96078a24ca50b9c9a38f2379e6be6d87171fcd400fc14c8934b3bce96e2f"
    ),
    node_pin!(
        "26.13.1",
        (26, 13, 1),
        "lithium",
        "linux-arm64",
        "80a330b35415382878df18d41afbd106ea89bfb3be0087d6608e289a14c5cc7d"
    ),
    node_pin!(
        "26.13.1",
        (26, 13, 1),
        "lithium",
        "linux-x64",
        "518674de2b621728a5e4486f5fb3b7d2900bb3cc4aa97969edaa463db99f9778"
    ),
    node_pin!(
        "24.22.3",
        (24, 22, 3),
        "krypton",
        "darwin-arm64",
        "ff6e17d4874b76d272212b908cc020d6e815fba5a6c7eaaa06add725840307fd"
    ),
    node_pin!(
        "24.22.3",
        (24, 22, 3),
        "krypton",
        "darwin-x64",
        "977614af0504c81045e7fd85596dbca0e5f951b96681516dadafedcca8c447f7"
    ),
    node_pin!(
        "24.22.3",
        (24, 22, 3),
        "krypton",
        "linux-arm64",
        "261af2af250dfd7e9f25cf151d1b2713a0adf8f10a9ad116c4947f68627affe8"
    ),
    node_pin!(
        "24.22.3",
        (24, 22, 3),
        "krypton",
        "linux-x64",
        "061ec3e3062115705b56b51fa9cc330ab50d34b6969387d4e4b1a50c6809acc2"
    ),
    node_pin!(
        "22.23.5",
        (22, 23, 5),
        "jod",
        "darwin-arm64",
        "e7131995759b61a0170b646010d2ad35a5d37b4cb0d471f3e8317eac6f742f3d"
    ),
    node_pin!(
        "22.23.5",
        (22, 23, 5),
        "jod",
        "darwin-x64",
        "c53383238206b9ae4db23c5d8a63645e2e753bec8ab51a49cc81939aa64e9422"
    ),
    node_pin!(
        "22.23.5",
        (22, 23, 5),
        "jod",
        "linux-arm64",
        "8bd3dc69eee8f3f226315a80d7d1d4faccf217a4c8fae500168ac2e38f3bdba3"
    ),
    node_pin!(
        "22.23.5",
        (22, 23, 5),
        "jod",
        "linux-x64",
        "c54118104acc524633b5abe106bbf44a53126ec8b4e5fa105cb92b5d7de8a1a1"
    ),
];

/// The listen preload, written into every Node install.
pub const LISTEN_PRELOAD: &str = include_str!("listen-loopback.cjs");

/// Node.js.
pub const NODE: Runtime = Runtime {
    name: "node",
    display: "Node",
    pins: PINS,
    pick: |dirs, pins| {
        let request = super::node_version::declared_in(dirs)?;
        super::node_version::choose(request.as_ref(), pins)
    },
    // A declaration darkmux cannot read (a link, `latest`) still says the
    // project asked for Node: it is refused, never skipped.
    declares: |dirs| !matches!(super::node_version::declared_in(dirs), Ok(None)),
    extras: &[Extra {
        path: "darkmux/listen-loopback.cjs",
        content: LISTEN_PRELOAD,
        env: Some(("NODE_OPTIONS", "--require")),
    }],
};
