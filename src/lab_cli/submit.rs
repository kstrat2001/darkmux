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
    let LabCmd::Submit { sub, runs, dry_run, out, site, replace, commit, engine, engine_backend, engine_version, quant, weights_dir, darkmux_version } = cmd
    else {
        unreachable!("cmd_lab_submit takes `lab submit` only")
    };
    if let Some(sub) = sub {
        return cmd_submit_token(sub);
    }
    let stated = submit::Stated { engine, engine_backend, engine_version, quant, weights_dir: weights_dir.map(Into::into), darkmux_version, commit };
    cmd_submit(SubmitArgs { runs, dry_run, out, site, replace, stated })
}

pub(crate) fn cmd_submit(args: SubmitArgs) -> Result<i32> {
    if args.runs.is_empty() {
        bail!("name the lab runs to share: `darkmux lab submit <run>...` (see `darkmux run list`)");
    }
    // Before any hashing: a submit with no token would do that work for nothing.
    let token = if args.dry_run { None } else { Some(stored_token()?) };

    let built = submit::gather(&args.runs, &args.stated, &mut |line| eprintln!("{line}"))?;
    for note in &built.notes {
        eprintln!("{note}");
    }
    let text = serde_json::to_string_pretty(&built.bundle)? + "\n";
    write_out(&args, &built, &text)?;
    let Some(token) = token else {
        println!("Dry run: nothing was sent.");
        return Ok(0);
    };
    let answer = send(&args, &token, &text)?;
    print_shared(&args.site, &built.fixture, &answer.body["data"]);
    Ok(0)
}

fn stored_token() -> Result<String> {
    upload::read_token().context("no upload token stored: make one on the site's account page, then `darkmux lab submit login`")
}

/// Writes the bundle to `--out`, or for a dry run to a file named for its first run.
fn write_out(args: &SubmitArgs, built: &submit::Built, text: &str) -> Result<()> {
    let out = args
        .out
        .clone()
        .map(PathBuf::from)
        .or_else(|| args.dry_run.then(|| PathBuf::from(format!("darkmux-evals-{}.json", file_safe(&args.runs[0])))));
    if let Some(path) = &out {
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
        println!("Wrote {}: {} run(s) of {}", path.display(), args.runs.len(), built.fixture);
    }
    Ok(())
}

/// Uploads the bundle; an answer outside 2xx is the site's refusal.
fn send(args: &SubmitArgs, token: &str, text: &str) -> Result<upload::Answer> {
    // curl reads the body from a file, removed after. It holds nothing secret.
    let body = std::env::temp_dir().join(format!("darkmux-evals-bundle-{}.json", std::process::id()));
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

fn print_shared(site: &str, fixture: &str, data: &serde_json::Value) {
    println!(
        "Shared {fixture} of {site}: {} added, {} already there, {} replaced.",
        data["added"].as_u64().unwrap_or(0),
        data["alreadyThere"].as_u64().unwrap_or(0),
        data["replaced"].as_u64().unwrap_or(0)
    );
    if let Some(url) = data["eval"]["url"].as_str() {
        println!("  eval: {url}");
    }
    for run in data["runs"].as_array().into_iter().flatten() {
        println!("  run: {} ({})", run["url"].as_str().unwrap_or("?"), run["state"].as_str().unwrap_or("?"));
    }
}

pub(crate) fn cmd_submit_token(sub: SubmitCmd) -> Result<i32> {
    match sub {
        SubmitCmd::Logout => {
            upload::forget_token()?;
            println!("Forgot the upload token.");
        }
        SubmitCmd::Login { site } => login(&site)?,
    }
    Ok(0)
}

/// Reads a token from stdin, checks it with the site, then stores it.
fn login(site: &str) -> Result<()> {
    if std::io::stdin().is_terminal() {
        eprintln!("Paste your upload token (from the site's account page), then press Enter:");
    }
    let mut token = String::new();
    std::io::stdin().read_line(&mut token).context("reading the token from stdin")?;
    let token = token.trim().to_string();
    if !upload::is_token(&token) {
        bail!("that is not an upload token (they start with {}); make one on the site's account page", upload::TOKEN_PREFIX);
    }
    let answer = upload::call(site, "GET", "/api/v1/account", &token, None)?;
    if answer.status != 200 {
        bail!("{site} did not accept the token: {}", answer.refusal());
    }
    let where_ = upload::store_token(&token)?;
    let data = &answer.body["data"];
    println!("Stored the upload token for {} in {where_}.", data["email"].as_str().unwrap_or("your account"));
    match (data["mayUpload"].as_bool(), data["reason"].as_str()) {
        (Some(true), _) => println!("This account may upload: `darkmux lab submit <run>...`."),
        (_, Some(reason)) => println!("This account cannot upload yet: {reason}"),
        _ => {}
    }
    Ok(())
}

/// A run id as part of a file name.
fn file_safe(run: &str) -> String {
    let name = std::path::Path::new(run).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '-' }).collect()
}
