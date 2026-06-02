pub mod artifact_dirs;
pub mod characterize;
pub mod compare;
pub mod cow_clone;
pub mod doctor;
pub mod fixture;
pub mod fixture_cli;
pub mod inspect;
pub mod instrument;
pub mod list;
pub mod profile_check;
// #463 workspace split — paths lifted into the darkmux-types foundation crate
// so crew can depend on path resolution without depending on lab. The
// re-export keeps all existing `crate::lab::paths::*` paths resolving unchanged.
pub use darkmux_types::paths;
pub mod registry;
pub mod run;
pub mod sandbox_hash;
pub mod tune;
