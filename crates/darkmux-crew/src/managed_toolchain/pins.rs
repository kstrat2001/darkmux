//! (#3561) One pinned build of a managed runtime: the row every runtime's
//! table is made of, keyed by (runtime, version, platform), with the
//! archive's URL, its SHA-256 and where in the archive the release folder
//! (the one holding `bin`) is. Only a pinned build is ever downloaded, and
//! it is installed only when its archive hashes to the pinned value; no
//! checksum is ever fetched at run time.

use std::fmt;

/// A release version, `major.minor.patch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Version {
            major,
            minor,
            patch,
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// One pinned build: a runtime's official archive of one version for one
/// platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pin {
    /// The runtime's name (`node`), [`super::Runtime::name`].
    pub runtime: &'static str,
    pub version: Version,
    /// The release line's name, lowercase (Node's LTS codename, as
    /// `.nvmrc`'s `lts/<codename>` names it).
    pub line: &'static str,
    /// The platform (`darwin-arm64`), as
    /// `darkmux_platform::Platform::node_release_platform` names this host.
    pub platform: &'static str,
    /// The official archive (a `.tar.gz`).
    pub url: &'static str,
    /// The archive's SHA-256, lowercase hex.
    pub sha256: &'static str,
    /// The folder in the archive that is the release (it holds `bin`).
    pub top: &'static str,
}

impl Pin {
    /// Its install's folder under `~/.darkmux/toolchains/`:
    /// `<runtime>-<version>-<platform>` (`node-26.13.1-darwin-arm64`).
    pub fn dir_name(&self) -> String {
        format!("{}-{}-{}", self.runtime, self.version, self.platform)
    }

    /// The archive's file name, the last part of its URL.
    pub fn archive(&self) -> &'static str {
        self.url.rsplit('/').next().unwrap_or(self.url)
    }
}
