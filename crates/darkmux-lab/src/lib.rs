//! darkmux-lab — the workload harness.
//!
//! Bundles the lab orchestrator (`lab`), the workload manifest/provider
//! registry (`workloads`), and the built-in providers (`providers`). These
//! three reference each other internally; their only outward deps are the
//! foundation crates (types/crew/eureka/profiles). Extracted in #515.

pub mod lab;
pub mod providers;
pub mod workloads;
