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
pub mod user_files;
pub mod workloads;

/// (#2928, lab decision) Every lab benchmark dispatch opts out of the live
/// channel: a lab run is a measurement, and nothing the channel does
/// (sampling, forwarding) may be charged to the measured dispatch or leave
/// the run. Checked on the source of EVERY file under `src/providers/` and
/// `src/lab/`, so a new provider that copies a work caller's
/// `live_channel: true` fails here. (The crawl mission, `src/crawl/`, is
/// operator-visible work and keeps the channel.)
#[cfg(test)]
mod live_channel_conformance {
    #[test]
    fn every_lab_dispatch_opts_out_of_the_live_channel() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut builders = 0;
        for dir in ["providers", "lab"] {
            for e in std::fs::read_dir(src.join(dir)).unwrap().flatten() {
                let path = e.path();
                if path.extension().and_then(|x| x.to_str()) != Some("rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap();
                let opts = text.matches("DispatchOpts {").count();
                if opts == 0 {
                    continue;
                }
                builders += opts;
                assert_eq!(text.matches("live_channel: false,").count(), opts, "{path:?}: every DispatchOpts must opt out");
                assert!(!text.contains("live_channel: true"), "{path:?} opts in");
            }
        }
        assert!(builders >= 3, "the scan sees the lab's dispatches ({builders})");
    }
}
