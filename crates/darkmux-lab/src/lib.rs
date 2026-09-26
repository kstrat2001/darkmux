//! darkmux-lab — the workload harness.
//!
//! Bundles the lab orchestrator (`lab`), the workload manifest/provider
//! registry (`workloads`), and the built-in providers (`providers`). These
//! three reference each other internally; their only outward deps are the
//! foundation crates (types/crew/profiles). Extracted in #515. (The crate
//! also carried an unused `darkmux-eureka` dependency — no code here ever
//! called it — dropped in the simplification batch.)
//!
//! `crawl` (#1959) is a fourth, independent member: the agentic bug
//! crawler's mechanical planning half (`plan`; `rules` lives in
//! `darkmux-crew` — promoted to a general template kind). It doesn't
//! reference `lab`/`workloads`/`providers` — crawling a workspace is not a
//! lab workload dispatch.

pub mod crawl;
pub mod lab;
pub mod providers;
pub mod workloads;

/// (#2928, lab decision) Every lab benchmark provider opts its dispatches
/// out of the live channel: a lab run is a measurement, and nothing the
/// channel does (sampling, forwarding) may be charged to the measured
/// dispatch or leave the run. Checked on the source so a new provider
/// that copies a work caller's `live_channel: true` fails here.
#[cfg(test)]
mod live_channel_conformance {
    #[test]
    fn every_lab_provider_opts_out_of_the_live_channel() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        for f in ["providers/prompt.rs", "providers/coding_task.rs", "providers/tool_bench.rs", "lab/review_bench.rs"] {
            let text = std::fs::read_to_string(dir.join(f)).unwrap();
            assert!(text.contains("DispatchOpts {"), "{f} builds DispatchOpts");
            assert!(text.contains("live_channel: false,"), "{f} must opt out of the live channel");
            assert!(!text.contains("live_channel: true"), "{f} opts in somewhere");
        }
    }
}
