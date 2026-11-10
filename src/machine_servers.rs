//! (#3450) The servers darkmux starts (Tensorfold, rec's Flint) on the
//! `machine` verbs: `machine status` lists them beside darkmux's LM Studio
//! loads and counts a profile served by one as loaded, and `machine eject`
//! stops them. Which server is darkmux's is its record matching the live
//! process (contract 4), read through `darkmux_profiles::engine::server`;
//! nothing here decides it again.

use darkmux_profiles::ownership::ServerEjectSummary;
use darkmux_serve::wire::EngineServer;
use darkmux_types::style;

/// `machine status`'s section for darkmux's servers, or nothing when none
/// runs. `now_ms` is the clock their uptime is read against.
pub(crate) fn status_section(servers: &[EngineServer], now_ms: u64) -> String {
    if servers.is_empty() {
        return String::new();
    }
    let mut out = format!("\n{}\n", style::header(&format!("darkmux engine servers ({}):", servers.len())));
    for s in servers {
        let state = if s.answering {
            format!("up {}", uptime(now_ms.saturating_sub(s.started_at_ms)))
        } else {
            "starting the model".to_string()
        };
        out.push_str(&format!(
            "  {} ctx={:<8} {} endpoint `{}` port {} pid {}  {}\n",
            style::accent(&format!("{:<46}", s.model)),
            s.n_ctx,
            s.engine,
            s.endpoint,
            s.port,
            s.pid,
            style::dim(&state),
        ));
    }
    out
}

/// An uptime in its two largest units: `45s`, `12m`, `3h12m`, `2d4h`.
fn uptime(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h{}m", s / 3600, s % 3600 / 60),
        _ => format!("{}d{}h", s / 86_400, s % 86_400 / 3600),
    }
}

/// Whether `m`, a model on an endpoint darkmux starts servers for, is served
/// now: darkmux's server on its endpoint runs its id at its declared context.
pub(crate) fn serves(servers: &[EngineServer], m: &darkmux_types::ProfileModel) -> bool {
    let endpoint = m.endpoint.as_ref().and_then(|e| e.named_id());
    servers.iter().any(|s| Some(s.endpoint.as_str()) == endpoint && s.model == m.id && m.n_ctx == Some(s.n_ctx))
}

/// Whether darkmux starts a server for `m` (its endpoint is Tensorfold or
/// rec's Flint), so its residency is a server, not an LM Studio load.
pub(crate) fn on_a_server(m: &darkmux_types::ProfileModel) -> bool {
    m.endpoint_kind().is_ok_and(|k| k.starts_servers())
}

/// `machine eject`'s lines for darkmux's servers, before the LM Studio half:
/// stdout and stderr, and whether a stop failed. Nothing when none runs.
pub(crate) fn eject_lines(summary: &ServerEjectSummary, dry_run: bool) -> (String, String, bool) {
    if summary.is_empty() {
        return (String::new(), String::new(), false);
    }
    let verb = if dry_run { "would stop" } else { "stop" };
    let mut out = String::new();
    for s in &summary.stopped {
        out.push_str(&format!("{verb} engine server `{}` ({}, pid {})\n", s.endpoint, s.model, s.pid));
    }
    for s in &summary.held {
        out.push_str(&format!("kept engine server `{}` ({}): a running dispatch holds it\n", s.endpoint, s.model));
    }
    for s in &summary.skipped {
        out.push_str(&format!("skipped engine server `{}` ({}): it was no longer running as darkmux's\n", s.endpoint, s.model));
    }
    out.push_str(&summary_line(summary, dry_run));
    let err: String = summary
        .failed
        .iter()
        .map(|f| format!("darkmux: ⚠ could not stop engine server `{}` — {}\n", f.identifier, f.error))
        .collect();
    (out, err, !summary.failed.is_empty())
}

/// The eject's one summary line for the servers.
fn summary_line(summary: &ServerEjectSummary, dry_run: bool) -> String {
    let mut line = format!("{} {} engine server(s)", if dry_run { "would stop" } else { "stopped" }, summary.stopped.len());
    let counts = [
        (summary.held.len(), "kept {} a running dispatch holds"),
        (summary.skipped.len(), "skipped {}"),
        (summary.failed.len(), "{} FAILED to stop"),
    ];
    for (n, words) in counts.into_iter().filter(|(n, _)| *n > 0) {
        line.push_str(&format!(", {}", words.replace("{}", &n.to_string())));
    }
    if dry_run {
        line.push_str(" [DRY RUN]");
    }
    line.push('\n');
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use darkmux_profiles::ownership::{EjectFailure, EjectedServer};

    fn server(endpoint: &str, model: &str, n_ctx: u32, answering: bool) -> EngineServer {
        EngineServer {
            endpoint: endpoint.into(),
            engine: "tensorfold".into(),
            model: model.into(),
            served_name: format!("darkmux:{model}"),
            n_ctx,
            port: 8091,
            pid: 4242,
            started_at_ms: 1_000_000,
            answering,
        }
    }

    fn on(endpoint: &str, id: &str, n_ctx: u32) -> darkmux_types::ProfileModel {
        darkmux_types::ProfileModel {
            id: id.into(),
            n_ctx: Some(n_ctx),
            endpoint: Some(darkmux_types::ModelEndpoint::reference(endpoint)),
            ..Default::default()
        }
    }

    /// (#3450) Each server is a line: model, context, engine, endpoint, port,
    /// pid, and its uptime, or "starting the model" while it does not answer.
    #[test]
    fn the_status_section_names_each_server_and_its_uptime() {
        let now = 1_000_000 + (3 * 3600 + 12 * 60 + 5) * 1000;
        let text = status_section(&[server("tf", "TensorFold/q38", 262_144, true), server("tg", "TensorFold/b", 8192, false)], now);
        assert!(text.contains("darkmux engine servers (2):"), "{text}");
        let tf = text.lines().find(|l| l.contains("TensorFold/q38")).unwrap();
        assert!(tf.contains("ctx=262144") && tf.contains("tensorfold endpoint `tf` port 8091 pid 4242") && tf.contains("up 3h12m"), "{tf}");
        let tg = text.lines().find(|l| l.contains("TensorFold/b")).unwrap();
        assert!(tg.contains("starting the model") && !tg.contains("up "), "{tg}");
        assert_eq!(status_section(&[], now), "", "no server, no section");
    }

    #[test]
    fn uptime_reads_in_its_two_largest_units() {
        assert_eq!(uptime(45_000), "45s");
        assert_eq!(uptime(12 * 60_000 + 5_000), "12m");
        assert_eq!(uptime((26 * 3600 + 30) * 1000), "1d2h");
    }

    /// (#3450) A model is served when darkmux's server on its endpoint runs
    /// its id at its context; another endpoint, model or context is not.
    #[test]
    fn a_model_is_served_by_its_endpoints_server_at_its_context() {
        let servers = [server("tf", "TensorFold/q38", 262_144, true)];
        assert!(serves(&servers, &on("tf", "TensorFold/q38", 262_144)));
        assert!(!serves(&servers, &on("tf", "TensorFold/q38", 8192)), "another context");
        assert!(!serves(&servers, &on("tg", "TensorFold/q38", 262_144)), "another endpoint");
        assert!(!serves(&servers, &on("tf", "TensorFold/other", 262_144)), "another model");
    }

    /// (#3450) What the eject did with each server, and a summary line.
    #[test]
    fn the_eject_lines_say_what_stopped_what_was_kept_and_what_failed() {
        let one = |e: &str| EjectedServer { endpoint: e.into(), model: format!("m/{e}"), pid: 7, without_lock: false };
        let summary = ServerEjectSummary {
            stopped: vec![one("tf")],
            held: vec![one("busy")],
            skipped: vec![one("gone")],
            failed: vec![EjectFailure { identifier: "stuck".into(), error: "boom".into() }],
        };
        let (out, err, failed) = eject_lines(&summary, false);
        assert_eq!(
            out,
            "stop engine server `tf` (m/tf, pid 7)\n\
             kept engine server `busy` (m/busy): a running dispatch holds it\n\
             skipped engine server `gone` (m/gone): it was no longer running as darkmux's\n\
             stopped 1 engine server(s), kept 1 a running dispatch holds, skipped 1, 1 FAILED to stop\n"
        );
        assert_eq!(err, "darkmux: ⚠ could not stop engine server `stuck` — boom\n");
        assert!(failed);
        let (dry, _, _) = eject_lines(&ServerEjectSummary { stopped: vec![one("tf")], ..Default::default() }, true);
        assert_eq!(dry, "would stop engine server `tf` (m/tf, pid 7)\nwould stop 1 engine server(s) [DRY RUN]\n");
        assert_eq!(eject_lines(&ServerEjectSummary::default(), false), (String::new(), String::new(), false));
    }
}
