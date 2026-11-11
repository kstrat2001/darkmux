//! (#3544) Pairing with no code to type: who may pair at once, and the
//! Allow dialog for everyone else.
//!
//! When a pairing request arrives, `client serve` asks which process on
//! this machine sent it (`peer_process_of`) and who signed that process
//! (`darkmux_platform::Platform::signed_app`: valid now, under Apple's
//! anchor, a team certificate, the hardened runtime). Then:
//!
//! - **Trusted** (a Mux Bot signed by one of `clients.pairing.verified_teams`,
//!   or an app the operator allowed before and darkmux remembered): approved
//!   at once through [`crate::client_pairing::approve_as`], the same path
//!   `client approve` takes, marked `auto_approved`, with a short notice.
//!   Just before approving, the connection is looked up again (it must
//!   still be that process's) and the signature is checked again.
//! - **Anything else** (an app signed by another team, an unsigned or ad hoc
//!   build, a container's traffic, a process darkmux cannot tell): a native
//!   dialog showing the code and who signed the app, or that no known
//!   developer did. Allow approves (and remembers a signed app's identity),
//!   Don't Allow denies, and no answer leaves the request pending to
//!   expire as before. The dialog runs on a thread of its own, never on the
//!   listener; a request raises at most one, a process at most
//!   [`PAIR_REQUESTS_PER_PROCESS`] at once and the machine at most
//!   [`MAX_OPEN_DIALOGS`].
//!
//! **What this is a boundary against.** Code running as this user can
//! already run `darkmux client approve` or edit `clients.json`, so this is
//! not a boundary against it: it decides when the operator need not be
//! asked. The boundaries are container traffic (Docker's forwarder never
//! verifies, so a container always gets the dialog and is never
//! remembered) and processes darkmux cannot see (another user's, which
//! never verify).
//!
//! **The residual pid window.** The listener is TCP on loopback, so the OS
//! hands it no credentials for the other end: the pid is whichever process
//! holds that connection when darkmux looks (`lsof`), and the signature is
//! read by pid. The re-check narrows the window to the time between the
//! last look and the store write (milliseconds), and the request's own
//! pairing secret ties the token to whoever sent the request. A process
//! that hands its open connection to a freshly launched Mux Bot (an
//! inherited descriptor) would still be approved as it; only a channel
//! whose peer the kernel names (a Unix socket's peer token, or XPC) closes
//! that, and that process could run `client approve` anyway.

use crate::client_pairing::{
    approve_as, deny, peer_process_of, Approval, ClientServerState, ClientStore, RememberedApp, SigningIdentity,
    PAIR_REQUESTS_PER_PROCESS,
};
use darkmux_platform::{AllowAnswer, SignedApp};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

/// Mux Bot's bundle ids: the app and its check build. A Mux Bot signed by a
/// verified team is one of these.
pub const MUX_BOT_BUNDLES: &[&str] = &["com.darklyenergized.darkmuxapp", "com.darklyenergized.darkmuxapp.check"];

/// How long a dialog waits for an answer before it gives up (the request
/// stays pending, and `client approve` can still approve it).
pub const DIALOG_TIMEOUT_SECS: u64 = 120;

/// Dialogs open at once on this machine, whoever asked.
pub const MAX_OPEN_DIALOGS: usize = 4;

/// `clients.pairing.*` as it reads now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingPolicy {
    /// `clients.pairing.verified_teams`.
    pub verified_teams: Vec<String>,
    /// `clients.pairing.dialog`.
    pub dialog: bool,
}

impl PairingPolicy {
    /// What an unreadable config leaves: nothing approved on its own, and
    /// the dialog (which only asks the operator).
    pub fn fail_closed() -> Self {
        Self { verified_teams: Vec::new(), dialog: true }
    }
}

/// `clients.pairing.*`, read per pairing request; `Err` when unreadable.
pub type PairingPolicySource = Arc<dyn Fn() -> Result<PairingPolicy, String> + Send + Sync>;
/// pid -> who signed it ([`darkmux_platform::Platform::signed_app`]).
pub type SignedAppCheck = Arc<dyn Fn(u32) -> Result<SignedApp, String> + Send + Sync>;
/// (message, the app's icon, timeout) -> the operator's answer
/// ([`darkmux_platform::Platform::ask_allow`]).
pub type AskAllow = Arc<dyn Fn(&str, Option<&std::path::Path>, u64) -> AllowAnswer + Send + Sync>;
/// A notice nobody waits on ([`darkmux_platform::Platform::notify`]).
pub type Notify = Arc<dyn Fn(&str) + Send + Sync>;

/// Why a trusted app is trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustedBy {
    /// A Mux Bot signed by a team in `clients.pairing.verified_teams`.
    VerifiedTeam,
    /// An app the operator allowed in the dialog.
    Remembered,
}

/// Who sent a pairing request, as far as darkmux can tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sender {
    /// Approved with no operator step, once it checks out again.
    Trusted(SignedApp, TrustedBy),
    /// Signed and named, but not trusted: the dialog asks, and Allow
    /// remembers it.
    Signed(SignedApp),
    /// Nothing darkmux can name: the dialog asks, and Allow remembers nothing.
    Unverified(String),
}

/// What `signed` (the sender's signature, or why it has none) means under
/// `policy` and the `remembered` apps.
pub fn judge(signed: Result<SignedApp, String>, policy: &PairingPolicy, remembered: &[RememberedApp]) -> Sender {
    let app = match signed {
        Ok(app) => app,
        Err(why) => return Sender::Unverified(why),
    };
    if policy.verified_teams.contains(&app.team) && MUX_BOT_BUNDLES.contains(&app.bundle.as_str()) {
        return Sender::Trusted(app, TrustedBy::VerifiedTeam);
    }
    if remembered.iter().any(|r| r.team == app.team && r.bundle == app.bundle) {
        return Sender::Trusted(app, TrustedBy::Remembered);
    }
    Sender::Signed(app)
}

/// The dialog's text: the code the app shows, and who signed the app as the
/// OS verified it (never the app's own name for itself), or that no known
/// developer did. Shown as plain text by the platform, never as script.
pub fn dialog_message(code: &str, signed: Option<&SignedApp>) -> String {
    let who = match signed {
        Some(app) => format!(
            "Signed by {}, team {}, {}.\nAllow pairs it now and next time.",
            app.signer, app.team, app.bundle
        ),
        None => "Not signed by a known developer.".to_string(),
    };
    format!("An app wants to use darkmux on this Mac. Code {code}.\n\n{who}")
}

/// The notice once a trusted app is paired.
pub fn paired_notice(app: &SignedApp, by: TrustedBy) -> String {
    match by {
        TrustedBy::VerifiedTeam => "Mux Bot is now paired with darkmux on this Mac.".to_string(),
        TrustedBy::Remembered => format!("{} is now paired with darkmux on this Mac.", app.bundle),
    }
}

/// The dialogs open now, by the process that asked (`None`: one darkmux
/// could not tell, all counted together).
#[derive(Default)]
pub struct DialogSlots(Mutex<HashMap<Option<u32>, usize>>);

/// One open dialog's place; given back when dropped.
pub struct DialogSlot(Arc<DialogSlots>, Option<u32>);

impl DialogSlots {
    /// A place for a dialog `from` asked for, unless that process has
    /// [`PAIR_REQUESTS_PER_PROCESS`] open or the machine [`MAX_OPEN_DIALOGS`].
    pub fn take(self: &Arc<Self>, from: Option<u32>) -> Option<DialogSlot> {
        let mut open = self.0.lock().unwrap();
        let total: usize = open.values().sum();
        let mine = open.entry(from).or_default();
        if *mine >= PAIR_REQUESTS_PER_PROCESS || total >= MAX_OPEN_DIALOGS {
            return None;
        }
        *mine += 1;
        Some(DialogSlot(self.clone(), from))
    }
}

impl Drop for DialogSlot {
    fn drop(&mut self) {
        let mut open = self.0 .0.lock().unwrap();
        if let Some(n) = open.get_mut(&self.1) {
            *n -= 1;
            if *n == 0 {
                open.remove(&self.1);
            }
        }
    }
}

/// Who sent a request from process `from` (`None`: darkmux could not tell),
/// under the pairing policy as it reads now. Off the async workers: it runs
/// `codesign`.
pub async fn sender_of(state: &ClientServerState, from: Option<u32>) -> (Sender, PairingPolicy) {
    let policy = (state.pairing_policy)().unwrap_or_else(|_| PairingPolicy::fail_closed());
    let Some(pid) = from else {
        return (Sender::Unverified("darkmux could not tell which process sent it".to_string()), policy);
    };
    let remembered = state.store.read().map(|f| f.remembered_apps).unwrap_or_default();
    let check = state.signed_app.clone();
    let signed = tokio::task::spawn_blocking(move || check(pid)).await.unwrap_or_else(|e| Err(e.to_string()));
    (judge(signed, &policy, &remembered), policy)
}

/// What a newly stored request gets: a trusted sender is approved now
/// (after [`still_the_same_app`]); anyone else gets the dialog, when the
/// policy shows one. Never waits on the operator. Serve-log lines.
pub async fn decide(
    state: &ClientServerState,
    request: (&str, &str),
    peer: Option<SocketAddr>,
    from: Option<u32>,
    (sender, policy): (Sender, PairingPolicy),
) -> Vec<String> {
    let (request_id, code) = request;
    let sender = match sender {
        Sender::Trusted(app, by) => match still_the_same_app(state, peer, from, &app).await {
            Ok(()) => return vec![approve_trusted(state, request_id, &app, by)],
            Err(why) => Sender::Unverified(format!("{app} did not check out again before approval: {why}")),
        },
        other => other,
    };
    match policy.dialog {
        true => offer_dialog(state, request_id, code, from, sender).into_iter().collect(),
        false => Vec::new(),
    }
}

/// The re-check immediately before an approval with no operator step: the
/// connection is still held by process `from`, and that process still
/// verifies as `app`.
async fn still_the_same_app(state: &ClientServerState, peer: Option<SocketAddr>, from: Option<u32>, app: &SignedApp) -> Result<(), String> {
    let pid = from.ok_or("no process")?;
    let now_from = peer_process_of(state, peer.map(axum::extract::ConnectInfo)).await;
    if now_from != Some(pid) {
        return Err(format!("its connection is now held by {now_from:?}, not pid {pid}"));
    }
    let check = state.signed_app.clone();
    let again = tokio::task::spawn_blocking(move || check(pid)).await.map_err(|e| e.to_string())??;
    match &again == app {
        true => Ok(()),
        false => Err(format!("pid {pid} is now {again}")),
    }
}

/// Approves a trusted sender's request through the one approval path, with
/// the notice. The serve-log line.
fn approve_trusted(state: &ClientServerState, request_id: &str, app: &SignedApp, by: TrustedBy) -> String {
    let how = Approval::Verified(SigningIdentity::from(app));
    match approve_as(&state.store, request_id, (state.now)(), &how) {
        Ok(name) => {
            (state.notify)(&paired_notice(app, by));
            format!("darkmux client serve: paired {name} with no dialog: signed by {app} ({})", app.signer)
        }
        Err(e) => format!("darkmux client serve: could not approve {request_id} for {app}: {e:#}"),
    }
}

/// Raises the dialog for a request on a thread of its own, when a slot is
/// free; a line for the serve log when none is.
fn offer_dialog(state: &ClientServerState, request_id: &str, code: &str, from: Option<u32>, sender: Sender) -> Option<String> {
    let Some(slot) = state.dialogs.take(from) else {
        return Some(format!(
            "darkmux client serve: no dialog for pairing request {request_id}: its process has dialogs open already; \
             `darkmux client approve --code {code}` approves it"
        ));
    };
    let remember = match &sender {
        Sender::Signed(app) | Sender::Trusted(app, _) => Some(app.clone()),
        Sender::Unverified(_) => None,
    };
    let message = dialog_message(code, remember.as_ref());
    // The app's own icon only for a signature that verified: an unverified
    // app's icon is its own choice, and could be Mux Bot's.
    let icon = remember.as_ref().and_then(|app| app.icon.clone());
    let (st, id) = (state.clone(), request_id.to_string());
    std::thread::spawn(move || {
        let _slot = slot;
        let answer = (st.ask_allow)(&message, icon.as_deref(), DIALOG_TIMEOUT_SECS);
        let remember = remember.as_ref().map(SigningIdentity::from);
        eprintln!("{}", act_on_answer(&st.store, &id, (st.now)(), answer, remember));
    });
    None
}

/// Carries out the dialog's answer for request `request_id`: Allow
/// approves (remembering `remember`), Don't Allow denies through the deny
/// path, and no answer leaves it pending. The serve-log line.
pub fn act_on_answer(store: &ClientStore, request_id: &str, now: u64, answer: AllowAnswer, remember: Option<SigningIdentity>) -> String {
    let said = match answer {
        AllowAnswer::Allow => approve_as(store, request_id, now, &Approval::Dialog { remember: remember.clone() })
            .map(|name| match remember {
                Some(app) => format!("approved as {name} in the dialog; {app} is remembered"),
                None => format!("approved as {name} in the dialog"),
            }),
        AllowAnswer::DontAllow => deny(store, request_id, now).map(|()| "denied in the dialog".to_string()),
        AllowAnswer::NoAnswer => Ok("no answer in the dialog; left pending".to_string()),
        AllowAnswer::NoDialog(why) => Ok(format!("no dialog ({why}); left pending for `darkmux client approve`")),
    };
    match said {
        Ok(s) => format!("darkmux client serve: pairing request {request_id}: {s}"),
        Err(e) => format!("darkmux client serve: pairing request {request_id}: {e:#}"),
    }
}
