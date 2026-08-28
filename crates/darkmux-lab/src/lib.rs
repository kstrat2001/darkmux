//! darkmux-lab — the workload harness.
//!
//! Bundles the lab orchestrator (`lab`), the workload manifest/provider
//! registry (`workloads`), and the built-in providers (`providers`). These
//! three reference each other internally; their only outward deps are the
//! foundation crates (types/crew/profiles). Extracted in #515. (The crate
//! also carried an unused `darkmux-eureka` dependency — no code here ever
//! called it — dropped in the simplification batch.)
//!
//! `crawl` (#1959 packet 1) is a fourth, independent member: the agentic
//! bug crawler's manifest/rules/sources/plan machinery. It doesn't
//! reference `lab`/`workloads`/`providers` — a corpus crawl is not a lab
//! workload dispatch.

pub mod crawl;
pub mod lab;
pub mod providers;
pub mod workloads;
