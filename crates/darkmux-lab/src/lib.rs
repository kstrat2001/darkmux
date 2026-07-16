//! darkmux-lab — the workload harness.
//!
//! Bundles the lab orchestrator (`lab`), the workload manifest/provider
//! registry (`workloads`), and the built-in providers (`providers`). These
//! three reference each other internally; their only outward deps are the
//! foundation crates (types/crew/profiles). Extracted in #515. (The crate
//! also carried an unused `darkmux-eureka` dependency — no code here ever
//! called it — dropped in the simplification batch.)

pub mod lab;
pub mod providers;
pub mod workloads;
