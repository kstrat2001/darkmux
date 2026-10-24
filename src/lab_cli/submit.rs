//! `darkmux lab submit` (and `login` / `logout`): sharing lab runs on the
//! darkmux evals site. The bundle and the calls live in
//! `darkmux_lab::lab::submit`; this is the command's reading and printing.

use anyhow::{bail, Context, Result};
use std::io::IsTerminal;
use std::path::PathBuf;

use crate::cli::{LabCmd, SubmitCmd};
use crate::lab::submit::{self, upload};

pub(crate) struct SubmitArgs {
    pub runs: Vec<String>,
    pub dry_run: bool,
    pub out: Option<String>,
    pub site: String,
    pub replace: bool,
    pub stated: submit::Stated,
}

/// `lab submit`, from its parsed command line: `login` / `logout`, or a submit.
pub(crate) fn cmd_lab_submit(cmd: LabCmd) -> Result<i32> {
    let LabCmd::Submit { sub, runs, dry_run, out, site, replace, commit, engine, engine_backend, engine_version, quant, weights_dir, darkmux_version, profiles } = cmd
    else {
        unreachable!("cmd_lab_submit takes `lab submit` only")
    };
    if let Some(sub) = sub {
        return cmd_submit_token(sub);
    }
    let stated = submit::Stated { engine, engine_backend, engine_version, quant, weights_dir: weights_dir.map(Into::into), darkmux_version, commit, profiles_file: profiles.profiles };
    cmd_submit(SubmitArgs { runs, dry_run, out, site, replace, stated })
}

pub(crate) fn cmd_submit(args: SubmitArgs) -> Result<i32> {
    if args.runs.is_empty() {
        bail!("name the lab runs to share: `darkmux lab submit <run>...` (see `darkmux run list`)");
    }
    // Before any hashing: a submit with no token would do that work for nothing.
    let token = if args.dry_run { None } else { Some(stored_token()?) };
    let built = submit::gather(&args.runs, &args.stated, &mut |line| eprintln!("{line}"))?;
    print!("{}", deliver(&args, &built, token.as_deref())?);
    Ok(0)
}

fn stored_token() -> Result<String> {
    upload::read_token().context("no upload token stored: make one on the site's account page, then `darkmux lab submit login`")
}

/// A built bundle's notes to stderr, the bundle to its file, and to the site
/// with a token (none is a dry run). Returns what to print.
fn deliver(args: &SubmitArgs, built: &submit::Built, token: Option<&str>) -> Result<String> {
    for note in &built.notes {
        eprintln!("{note}");
    }
    let text = serde_json::to_string_pretty(&built.bundle)? + "\n";
    let wrote = write_out(args, built, &text)?;
    let Some(token) = token else {
        return Ok(format!("{wrote}Dry run: nothing was sent.\n"));
    };
    let answer = send(args, token, &text)?;
    Ok(wrote + &shared_text(&args.site, &built.fixture, &answer.body["data"]))
}

/// Where the bundle is written: `--out`, or for a dry run a file named for its first run.
fn out_path(args: &SubmitArgs) -> Option<PathBuf> {
    args.out
        .clone()
        .map(PathBuf::from)
        .or_else(|| args.dry_run.then(|| PathBuf::from(format!("darkmux-evals-{}.json", file_safe(&args.runs[0])))))
}

/// Writes the bundle to [`out_path`], saying so.
fn write_out(args: &SubmitArgs, built: &submit::Built, text: &str) -> Result<String> {
    let Some(path) = out_path(args) else {
        return Ok(String::new());
    };
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(format!("Wrote {}: {} run(s) of {}\n", path.display(), args.runs.len(), built.fixture))
}

/// Uploads the bundle; an answer outside 2xx is the site's refusal.
fn send(args: &SubmitArgs, token: &str, text: &str) -> Result<upload::Answer> {
    // curl reads the body from a file, removed after. It holds nothing secret.
    static SENT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SENT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let body = std::env::temp_dir().join(format!("darkmux-evals-bundle-{}-{n}.json", std::process::id()));
    upload::write_private(&body, text.as_bytes())?;
    let path = if args.replace { "/api/v1/uploads?replace=1" } else { "/api/v1/uploads" };
    let answer = upload::call(&args.site, "POST", path, token, Some(&body));
    let _ = std::fs::remove_file(&body);
    let answer = answer?;
    if !(200..300).contains(&answer.status) {
        bail!("{} refused the upload: {}", args.site, answer.refusal());
    }
    Ok(answer)
}

/// What the site took, as printed.
fn shared_text(site: &str, fixture: &str, data: &serde_json::Value) -> String {
    let mut text = format!(
        "Shared {fixture} of {site}: {} added, {} already there, {} replaced.\n",
        data["added"].as_u64().unwrap_or(0),
        data["alreadyThere"].as_u64().unwrap_or(0),
        data["replaced"].as_u64().unwrap_or(0)
    );
    if let Some(url) = data["eval"]["url"].as_str() {
        text.push_str(&format!("  eval: {url}\n"));
    }
    for run in data["runs"].as_array().into_iter().flatten() {
        text.push_str(&format!("  run: {} ({})\n", run["url"].as_str().unwrap_or("?"), run["state"].as_str().unwrap_or("?")));
    }
    text
}

pub(crate) fn cmd_submit_token(sub: SubmitCmd) -> Result<i32> {
    let stdin = std::io::stdin();
    let prompt = stdin.is_terminal();
    print!("{}", token_command(sub, &mut stdin.lock(), prompt, upload::store_token, upload::forget_token)?);
    Ok(0)
}

/// `login` (a token read from `input`, kept with `store`) or `logout`
/// (`forget`). Returns what to print.
fn token_command(
    sub: SubmitCmd,
    input: &mut dyn std::io::BufRead,
    prompt: bool,
    store: impl FnOnce(&str) -> Result<String>,
    forget: impl FnOnce() -> Result<()>,
) -> Result<String> {
    let SubmitCmd::Login { site } = sub else {
        forget()?;
        return Ok("Forgot the upload token.\n".to_string());
    };
    if prompt {
        eprintln!("Paste your upload token (from the site's account page), then press Enter:");
    }
    let mut line = String::new();
    input.read_line(&mut line).context("reading the token from stdin")?;
    login_with(&site, &line, store)
}

/// Checks a pasted token with the site, then keeps it with `store` (which
/// says where). Returns what to print.
fn login_with(site: &str, line: &str, store: impl FnOnce(&str) -> Result<String>) -> Result<String> {
    let token = line.trim();
    if !upload::is_token(token) {
        bail!("that is not an upload token (they start with {}); make one on the site's account page", upload::TOKEN_PREFIX);
    }
    let answer = upload::call(site, "GET", "/api/v1/account", token, None)?;
    if answer.status != 200 {
        bail!("{site} did not accept the token: {}", answer.refusal());
    }
    let where_ = store(token)?;
    let data = &answer.body["data"];
    let mut text = format!("Stored the upload token for {} in {where_}.\n", data["email"].as_str().unwrap_or("your account"));
    match (data["mayUpload"].as_bool(), data["reason"].as_str()) {
        (Some(true), _) => text.push_str("This account may upload: `darkmux lab submit <run>...`.\n"),
        (_, Some(reason)) => text.push_str(&format!("This account cannot upload yet: {reason}\n")),
        _ => {}
    }
    Ok(text)
}

/// A run id as part of a file name.
fn file_safe(run: &str) -> String {
    let name = std::path::Path::new(run).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '-' }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The engine's details describe the engine named with --engine; alone
    /// they are refused, never accepted and dropped.
    #[test]
    fn engine_details_need_the_engine() {
        use clap::Parser;
        let parse = |extra: &[&str]| {
            let mut argv = vec!["darkmux", "lab", "submit", "r", "--dry-run"];
            argv.extend_from_slice(extra);
            crate::cli::Cli::try_parse_from(argv)
        };
        for flag in [["--quant", "Q4_K_M"], ["--engine-version", "0.4.1"], ["--engine-backend", "metal"]] {
            let err = parse(&flag).err().unwrap_or_else(|| panic!("{} alone must be refused", flag[0])).to_string();
            assert!(err.contains("--engine"), "{err}");
            assert!(parse(&[flag[0], flag[1], "--engine", "rec"]).is_ok(), "{} with --engine", flag[0]);
        }
    }

    fn token() -> String {
        format!("{}{}", upload::TOKEN_PREFIX, "ab12".repeat(8))
    }

    /// A one-request HTTP server on this machine: answers `status` and
    /// `body`, and hands back the request line it read.
    fn serve_once(status: u16, body: &'static str) -> (String, std::thread::JoinHandle<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let site = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let handle = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            conn.set_read_timeout(Some(std::time::Duration::from_millis(500))).unwrap();
            let mut got = Vec::new();
            let mut buf = [0u8; 4096];
            while let Ok(n) = conn.read(&mut buf) {
                if n == 0 {
                    break;
                }
                got.extend_from_slice(&buf[..n]);
            }
            let reply = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            conn.write_all(reply.as_bytes()).unwrap();
            String::from_utf8_lossy(&got).lines().next().unwrap_or_default().to_string()
        });
        (site, handle)
    }

    fn args(site: &str, dry_run: bool, out: Option<String>) -> SubmitArgs {
        SubmitArgs { runs: vec!["/lab/runs/r 1".into()], dry_run, out, site: site.into(), replace: false, stated: Default::default() }
    }

    fn built() -> submit::Built {
        submit::Built { bundle: json!({"runs": []}), notes: vec!["r1: passed".into()], fixture: "o/r@abc1234 (f 1)".into() }
    }

    #[test]
    fn a_run_id_becomes_a_safe_file_name() {
        assert_eq!(file_safe("/lab/runs/2026-10-22 r:1"), "2026-10-22-r-1");
        assert_eq!(file_safe("run_1.a"), "run_1.a");
        assert_eq!(file_safe("/"), "");
    }

    #[test]
    fn the_bundle_goes_to_out_or_for_a_dry_run_beside_the_cwd() {
        assert_eq!(out_path(&args("s", false, None)), None, "a submit writes no file unless asked");
        assert_eq!(out_path(&args("s", true, None)), Some(PathBuf::from("darkmux-evals-r-1.json")));
        assert_eq!(out_path(&args("s", true, Some("b.json".into()))), Some(PathBuf::from("b.json")));
    }

    #[test]
    fn a_dry_run_writes_the_bundle_and_sends_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("bundle.json");
        let said = deliver(&args("http://127.0.0.1:9", true, Some(out.display().to_string())), &built(), None).unwrap();
        assert_eq!(said, format!("Wrote {}: 1 run(s) of o/r@abc1234 (f 1)\nDry run: nothing was sent.\n", out.display()));
        assert_eq!(serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(&out).unwrap()).unwrap(), json!({"runs": []}));
        let unwritable = args("s", true, Some(dir.path().join("no/such/dir.json").display().to_string()));
        assert!(deliver(&unwritable, &built(), None).unwrap_err().to_string().contains("writing"));
    }

    #[test]
    fn a_submit_uploads_and_says_what_the_site_took() {
        let (site, server) = serve_once(
            201,
            r#"{"data":{"added":1,"alreadyThere":0,"replaced":0,"eval":{"url":"https://e/x"},"runs":[{"url":"https://e/r","state":"pending"}]}}"#,
        );
        let mut a = args(&site, false, None);
        a.replace = true;
        let said = deliver(&a, &built(), Some(&token())).unwrap();
        let request = server.join().unwrap();
        assert!(request.starts_with("POST /api/v1/uploads?replace=1 "), "{request}");
        assert_eq!(said, format!("Shared o/r@abc1234 (f 1) of {site}: 1 added, 0 already there, 0 replaced.\n  eval: https://e/x\n  run: https://e/r (pending)\n"));
    }

    #[test]
    fn a_refused_upload_is_the_sites_words() {
        let (site, server) = serve_once(403, r#"{"error":"Uploads are not open yet"}"#);
        let err = deliver(&args(&site, false, None), &built(), Some(&token())).unwrap_err().to_string();
        assert_eq!(err, format!("{site} refused the upload: Uploads are not open yet (HTTP 403)"));
        assert!(server.join().unwrap().starts_with("POST /api/v1/uploads "));
    }

    #[test]
    fn what_the_site_took_reads_with_nothing_listed() {
        assert_eq!(shared_text("s", "f", &serde_json::Value::Null), "Shared f of s: 0 added, 0 already there, 0 replaced.\n");
    }

    #[test]
    fn a_submit_without_runs_names_what_to_pass() {
        let err = cmd_submit(SubmitArgs { runs: vec![], ..args("s", true, None) }).unwrap_err().to_string();
        assert!(err.contains("name the lab runs to share"));
    }

    #[test]
    fn login_checks_the_token_with_the_site_before_keeping_it() {
        let (site, server) = serve_once(200, r#"{"data":{"email":"me@example.com","mayUpload":true}}"#);
        let mut kept = None;
        let said = login_with(&site, &format!("{}\n", token()), |t| {
            kept = Some(t.to_string());
            Ok("the test store".into())
        })
        .unwrap();
        assert!(server.join().unwrap().starts_with("GET /api/v1/account "));
        assert_eq!(kept, Some(token()));
        assert_eq!(said, "Stored the upload token for me@example.com in the test store.\nThis account may upload: `darkmux lab submit <run>...`.\n");

        let (site, _server) = serve_once(200, r#"{"data":{"mayUpload":false,"reason":"uploads open soon"}}"#);
        let said = login_with(&site, &token(), |_| Ok("x".into())).unwrap();
        assert!(said.contains("your account") && said.ends_with("This account cannot upload yet: uploads open soon\n"), "{said}");

        let (site, _server) = serve_once(200, r#"{"data":{}}"#);
        assert!(!login_with(&site, &token(), |_| Ok("x".into())).unwrap().contains("upload:"));
    }

    #[test]
    fn logout_forgets_and_login_reads_the_token_from_its_input() {
        let mut forgot = false;
        let said = token_command(SubmitCmd::Logout, &mut std::io::empty(), false, |_| unreachable!(), || {
            forgot = true;
            Ok(())
        })
        .unwrap();
        assert!(forgot);
        assert_eq!(said, "Forgot the upload token.\n");

        let (site, server) = serve_once(200, r#"{"data":{"email":"me@example.com"}}"#);
        let mut input = std::io::Cursor::new(format!("{}\nignored\n", token()));
        let said = token_command(SubmitCmd::Login { site }, &mut input, true, |_| Ok("the test store".into()), || unreachable!()).unwrap();
        assert!(said.starts_with("Stored the upload token for me@example.com in the test store."), "{said}");
        assert!(server.join().unwrap().starts_with("GET /api/v1/account "));

        let empty = token_command(SubmitCmd::Login { site: "http://127.0.0.1:9".into() }, &mut std::io::empty(), false, |_| unreachable!(), || unreachable!());
        assert!(empty.unwrap_err().to_string().contains("not an upload token"), "nothing pasted is no token");
    }

    #[test]
    fn login_keeps_nothing_the_site_refused() {
        assert!(login_with("http://127.0.0.1:9", "not a token", |_| unreachable!()).unwrap_err().to_string().contains("not an upload token"));
        let (site, _server) = serve_once(401, r#"{"error":"Unknown token"}"#);
        let err = login_with(&site, &token(), |_| unreachable!("a refused token is never stored")).unwrap_err().to_string();
        assert_eq!(err, format!("{site} did not accept the token: Unknown token (HTTP 401)"));
    }
}
