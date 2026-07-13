pub mod artifact_dirs;
// (#1222 Phase B packet 3) Built-in review bundler — diff -> per-changed-
// function code bundles + mechanical facts + manifest.
pub mod bundle;
pub mod characterize;
pub mod compare;
pub mod cow_clone;
// (#1222) Dialectic (adversarial) review orchestration — review-bench's
// prosecutor → defender → judge mode.
pub mod dialectic;
pub mod doctor;
pub mod fixture;
pub mod fixture_cli;
pub mod inspect;
pub mod list;
pub mod loop_report;
pub mod profile_check;
// #463 workspace split — paths lifted into the darkmux-types foundation crate
// so crew can depend on path resolution without depending on lab. The
// re-export keeps all existing `crate::lab::paths::*` paths resolving unchanged.
pub use darkmux_types::paths;
pub mod registry;
// (#1222 Phase B packet 4; renamed from `funnel` in #1349 — the earlier
// name described a retired bespoke execution mechanism this driver no
// longer needs) The PR-review pipeline driver — bundles → probe (k draws)
// → dedup → double-confirm judge → envelope.
pub mod review;
pub mod review_bench;
pub mod run;
pub mod sandbox_hash;
// (#1198) scores.json — the bench suite's persisted score artifact (#1197).
pub mod scores;
pub mod tune;
