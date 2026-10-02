//! (#2916 stage 2) Which submitted jobs may run at once on a receiver.
//!
//! Busy is decided by what a job INVOKES, not by the machine as a whole:
//!
//! - a job on a **local** model holds that model for its whole run. LM Studio
//!   serves one request at a time per instance (measured 2026-09-26, #2914),
//!   so a second job for the same model is busy, while a job for a different
//!   local model is not;
//! - a job on an endpoint darkmux does **not manage** runs beside other jobs
//!   on that endpoint up to its `limits.concurrent_calls` (`0` = unbounded,
//!   the darkmux bound convention), and one at a time when it declares none
//!   (#3035). Jobs on different endpoints never wait on each other.
//!
//! Past either limit the receiver's `fleet.busy_policy` decides: `refuse`
//! answers at once, naming what is running ([`SeatBook::try_claim`]'s
//! [`Occupied`]); `queue` waits for the seat ([`SeatBook::claim_waiting`]),
//! first come first served per seat. A newcomer never overtakes a job
//! already waiting for the same seat.
//!
//! Scope, stated so it is not mistaken for more: the book counts the jobs
//! OTHER machines submitted. This machine's own dispatches do not register
//! here; they meet a busy model inside LM Studio as before.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// What a submitted job invokes, as the receiver resolved it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkSeat {
    /// A model this machine loads and serves itself (a managed endpoint).
    Local { model: String },
    /// An endpoint this machine only sends requests to (an unmanaged one):
    /// its seat key (an id, or the URL of an inline endpoint), the name that
    /// may be shown to a peer, the model asked of it, and its declared
    /// `limits.concurrent_calls`. The key is internal and may carry
    /// credentials; only `label` ever reaches a peer.
    Unmanaged { endpoint: String, label: String, model: String, concurrent_calls: Option<u32> },
}

impl WorkSeat {
    /// The key waiters queue on: one per local model, one per unmanaged
    /// endpoint.
    fn key(&self) -> String {
        match self {
            WorkSeat::Local { model } => format!("local:{model}"),
            WorkSeat::Unmanaged { endpoint, .. } => format!("endpoint:{endpoint}"),
        }
    }

    /// How many jobs may hold this seat's key at once.
    fn width(&self) -> usize {
        match self {
            WorkSeat::Local { .. } => 1,
            WorkSeat::Unmanaged { concurrent_calls, .. } => {
                concurrent_calls.map_or(1, |n| darkmux_types::config_access::jobs_at_once(n as usize))
            }
        }
    }
}

/// How a wait for a seat ended.
pub enum Waited {
    /// The job holds its seat now.
    Seat(SeatGuard),
    /// The sender stopped waiting; the job left the queue and never ran.
    Cancelled,
    /// The job waited as long as it may; it left the queue and never ran.
    /// Carries what held the seat.
    TimedOut(Occupied),
}

/// Why a seat is not free: a sentence naming what is running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Occupied {
    pub what: String,
}

/// One claim on a seat: a unique ticket and the session that took it. A
/// session id is not unique (the receiver derives it from the sender's, so a
/// retry repeats it), so a guard frees by ticket.
struct Held {
    ticket: u64,
    session: String,
}

#[derive(Default)]
struct Book {
    /// Local model -> the session running on it.
    local: BTreeMap<String, Held>,
    /// Endpoint -> the claims running on it.
    unmanaged: BTreeMap<String, Vec<Held>>,
    /// Waiters in arrival order: (seat key, ticket).
    waiting: VecDeque<(String, u64)>,
    next_ticket: u64,
}

/// The receiver's record of running submitted jobs.
pub struct SeatBook {
    book: Mutex<Book>,
    freed: Condvar,
}

/// What the book holds right now, for a machine card: the seats taken and how
/// many jobs wait, and nothing about who holds them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatSnapshot {
    /// The local models a submitted job holds, sorted.
    pub local_held: Vec<String>,
    /// Jobs running on unmanaged endpoints, all endpoints together.
    pub unmanaged_held: usize,
    /// Jobs waiting for a seat.
    pub waiting: usize,
}

/// Holds a seat; dropping it frees the seat and wakes the waiters.
pub struct SeatGuard {
    owner: Arc<SeatBook>,
    seat: WorkSeat,
    ticket: u64,
}

impl Drop for SeatGuard {
    fn drop(&mut self) {
        let mut b = self.owner.book.lock().unwrap_or_else(|p| p.into_inner());
        match &self.seat {
            WorkSeat::Local { model } => {
                if b.local.get(model).is_some_and(|h| h.ticket == self.ticket) {
                    b.local.remove(model);
                }
            }
            WorkSeat::Unmanaged { endpoint, .. } => {
                if let Some(held) = b.unmanaged.get_mut(endpoint) {
                    held.retain(|h| h.ticket != self.ticket);
                    if held.is_empty() {
                        b.unmanaged.remove(endpoint);
                    }
                }
            }
        }
        drop(b);
        self.owner.freed.notify_all();
    }
}

impl Default for SeatBook {
    fn default() -> Self {
        Self::new()
    }
}

impl SeatBook {
    pub fn new() -> Self {
        Self { book: Mutex::new(Book::default()), freed: Condvar::new() }
    }

    /// Why `seat` cannot be taken now, or `None` when it can.
    fn occupied(&self, b: &Book, seat: &WorkSeat) -> Option<Occupied> {
        match seat {
            WorkSeat::Local { model } => b.local.get(model).map(|h| Occupied { what: format!("{} is running on {model}", h.session) }),
            WorkSeat::Unmanaged { endpoint, label, concurrent_calls, .. } => {
                let held = b.unmanaged.get(endpoint).map(Vec::as_slice).unwrap_or_default();
                (held.len() >= seat.width()).then(|| Occupied {
                    what: format!(
                        "{} job(s) are running on endpoint {label} ({}), {}",
                        held.len(),
                        held.iter().map(|h| h.session.as_str()).collect::<Vec<_>>().join(", "),
                        match concurrent_calls {
                            Some(n) => format!("its `limits.concurrent_calls` ({n})"),
                            None => "which declares no `limits.concurrent_calls`, so its calls run one at a time".to_string(),
                        }
                    ),
                })
            }
        }
    }

    fn take(self: &Arc<Self>, b: &mut Book, seat: &WorkSeat, session: &str) -> SeatGuard {
        let ticket = b.next_ticket;
        b.next_ticket += 1;
        let held = Held { ticket, session: session.to_string() };
        match seat {
            WorkSeat::Local { model } => {
                b.local.insert(model.clone(), held);
            }
            WorkSeat::Unmanaged { endpoint, .. } => b.unmanaged.entry(endpoint.clone()).or_default().push(held),
        }
        SeatGuard { owner: Arc::clone(self), seat: seat.clone(), ticket }
    }

    /// Why a newcomer for `seat` cannot take it now: the seat is held, or an
    /// earlier job is waiting for the same seat (a newcomer never overtakes
    /// the queue). `None` when it can.
    fn blocker(&self, b: &Book, seat: &WorkSeat) -> Option<Occupied> {
        self.occupied(b, seat).or_else(|| {
            let key = seat.key();
            b.waiting
                .iter()
                .any(|(k, _)| *k == key)
                .then(|| Occupied { what: "earlier jobs are waiting for the same seat".into() })
        })
    }

    /// Take `seat` now, or say what holds it.
    pub fn try_claim(self: &Arc<Self>, seat: &WorkSeat, session: &str) -> Result<SeatGuard, Occupied> {
        let mut b = self.book.lock().unwrap_or_else(|p| p.into_inner());
        match self.blocker(&b, seat) {
            Some(o) => Err(o),
            None => Ok(self.take(&mut b, seat, session)),
        }
    }

    /// What [`try_claim`](Self::try_claim) would answer right now, without
    /// taking the seat: `None` when it is free. For a check, which must not
    /// hold a seat a real job could be refused for.
    pub fn peek(&self, seat: &WorkSeat) -> Option<Occupied> {
        let b = self.book.lock().unwrap_or_else(|p| p.into_inner());
        self.blocker(&b, seat)
    }

    /// Wait for `seat`, first come first served, then take it.
    ///
    /// While it waits, `on_wait` hears what holds the seat right away and
    /// again every `heartbeat`, so a waiting sender hears from this machine
    /// before its read deadline. It stops waiting, leaving the queue and
    /// taking nothing, when `cancelled()` turns true (the sender hung up) or
    /// `deadline` passes (the job would outlive its connection or its
    /// maximum queue age).
    pub fn claim_waiting(
        self: &Arc<Self>,
        seat: &WorkSeat,
        session: &str,
        heartbeat: Duration,
        deadline: Instant,
        cancelled: &dyn Fn() -> bool,
        mut on_wait: impl FnMut(&Occupied),
    ) -> Waited {
        let key = seat.key();
        let mut b = self.book.lock().unwrap_or_else(|p| p.into_inner());
        let ticket = b.next_ticket;
        b.next_ticket += 1;
        b.waiting.push_back((key.clone(), ticket));
        let mut announce = true;
        loop {
            if cancelled() {
                self.leave(&mut b, ticket);
                return Waited::Cancelled;
            }
            let first_for_key = b.waiting.iter().find(|(k, _)| *k == key).map(|(_, t)| *t) == Some(ticket);
            let held = self.occupied(&b, seat);
            if first_for_key && held.is_none() {
                b.waiting.retain(|(_, t)| *t != ticket);
                let guard = self.take(&mut b, seat, session);
                drop(b);
                // Wake the next waiter for another key that may now move.
                self.freed.notify_all();
                return Waited::Seat(guard);
            }
            let what = held.unwrap_or_else(|| Occupied { what: "earlier jobs are waiting for the same seat".into() });
            let now = Instant::now();
            if now >= deadline {
                self.leave(&mut b, ticket);
                return Waited::TimedOut(what);
            }
            if announce {
                announce = false;
                drop(b);
                on_wait(&what);
                b = self.book.lock().unwrap_or_else(|p| p.into_inner());
                continue;
            }
            let (next, timeout) =
                self.freed.wait_timeout(b, heartbeat.min(deadline - now)).unwrap_or_else(|p| p.into_inner());
            b = next;
            // A heartbeat: say again what it waits for, if it still waits.
            announce = timeout.timed_out();
        }
    }

    /// Take `ticket` out of the queue and wake the others: the one behind
    /// it may now be first.
    fn leave(&self, b: &mut Book, ticket: u64) {
        b.waiting.retain(|(_, t)| *t != ticket);
        self.freed.notify_all();
    }

    /// The seats taken and the jobs waiting, at this moment.
    pub fn snapshot(&self) -> SeatSnapshot {
        let b = self.book.lock().unwrap_or_else(|p| p.into_inner());
        SeatSnapshot {
            local_held: b.local.keys().cloned().collect(),
            unmanaged_held: b.unmanaged.values().map(Vec::len).sum(),
            waiting: b.waiting.len(),
        }
    }

    /// Sessions running now (for tests and logs).
    pub fn running(&self) -> Vec<String> {
        let b = self.book.lock().unwrap_or_else(|p| p.into_inner());
        b.local.values().map(|h| h.session.clone()).chain(b.unmanaged.values().flatten().map(|h| h.session.clone())).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(m: &str) -> WorkSeat {
        WorkSeat::Local { model: m.into() }
    }
    fn endpoint(id: &str, concurrent_calls: Option<u32>) -> WorkSeat {
        WorkSeat::Unmanaged { endpoint: id.into(), label: id.into(), model: "gpt-x".into(), concurrent_calls }
    }

    fn far() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    fn never() -> bool {
        false
    }

    fn seat_of(w: Waited) -> SeatGuard {
        match w {
            Waited::Seat(g) => g,
            Waited::Cancelled => panic!("cancelled"),
            Waited::TimedOut(o) => panic!("timed out behind {o:?}"),
        }
    }

    #[test]
    fn the_snapshot_names_held_seats_and_no_sessions() {
        let book = Arc::new(SeatBook::new());
        let empty = book.snapshot();
        assert_eq!(empty, SeatSnapshot { local_held: vec![], unmanaged_held: 0, waiting: 0 });
        let _a = book.try_claim(&local("qwen-35b"), "s1").expect("free");
        let _h = book.try_claim(&endpoint("azure", None), "s2").expect("free");
        let snap = book.snapshot();
        assert_eq!(snap.local_held, vec!["qwen-35b".to_string()]);
        assert_eq!(snap.unmanaged_held, 1);
        assert!(!format!("{snap:?}").contains("s1"), "a session id must not reach the snapshot");
        drop(_a);
        assert!(book.snapshot().local_held.is_empty(), "a freed seat leaves the snapshot");
    }

    #[test]
    fn one_job_per_local_model_and_different_models_run_together() {
        let book = Arc::new(SeatBook::new());
        let a = book.try_claim(&local("qwen-35b"), "s1").expect("free");
        let busy = book.try_claim(&local("qwen-35b"), "s2").err().expect("same model is busy");
        assert!(busy.what.contains("s1") && busy.what.contains("qwen-35b"), "names what is running: {busy:?}");
        let _b = book.try_claim(&local("qwen-4b"), "s3").expect("a different local model is not busy");
        drop(a);
        assert!(book.try_claim(&local("qwen-35b"), "s4").is_ok(), "the seat frees when the job ends");
    }

    /// (#3035) Jobs on one unmanaged endpoint run together up to its
    /// `limits.concurrent_calls`, and the refusal names the endpoint, who holds
    /// it and the field that bounds it.
    #[test]
    fn jobs_on_an_endpoint_run_together_up_to_its_concurrent_calls() {
        let book = Arc::new(SeatBook::new());
        let azure = endpoint("azure", Some(2));
        let _a = book.try_claim(&azure, "h1").unwrap();
        let _b = book.try_claim(&azure, "h2").expect("second job fits the declared concurrency");
        let busy = book.try_claim(&azure, "h3").err().expect("past the limit");
        assert!(busy.what.contains("limits.concurrent_calls") && busy.what.contains("h1, h2") && busy.what.contains("azure"), "{busy:?}");
        // An endpoint's jobs never block a local one, and the reverse.
        assert!(book.try_claim(&local("m"), "l1").is_ok());
    }

    /// (#3035) An endpoint that declares no `limits.concurrent_calls` takes
    /// one job at a time, and says why; another endpoint is not affected.
    #[test]
    fn an_endpoint_with_no_declared_concurrency_takes_one_job_at_a_time() {
        let book = Arc::new(SeatBook::new());
        let _a = book.try_claim(&endpoint("azure", None), "h1").unwrap();
        let busy = book.try_claim(&endpoint("azure", None), "h2").err().expect("serial");
        assert!(busy.what.contains("declares no `limits.concurrent_calls`") && busy.what.contains("h1"), "{busy:?}");
        assert!(book.try_claim(&endpoint("openai", None), "h3").is_ok(), "another endpoint is its own seat");
    }

    /// (#3035) A queue is per endpoint: a job waiting for one endpoint's seat
    /// holds a newcomer for THAT endpoint behind it, never one for another.
    #[test]
    fn a_waiter_for_one_endpoint_does_not_hold_another_endpoints_newcomers() {
        let book = Arc::new(SeatBook::new());
        book.book.lock().unwrap().waiting.push_back((endpoint("azure", None).key(), 7));
        let behind = book.try_claim(&endpoint("azure", None), "h1").err().expect("behind the waiter");
        assert!(behind.what.contains("waiting"), "{behind:?}");
        assert!(book.try_claim(&endpoint("openai", None), "h2").is_ok(), "another endpoint's queue is not this one's");
    }

    #[test]
    fn a_zero_concurrent_calls_means_unbounded_jobs() {
        let book = Arc::new(SeatBook::new());
        let guards: Vec<_> =
            (0..20).map(|i| book.try_claim(&endpoint("azure", Some(0)), &format!("h{i}")).unwrap()).collect();
        assert_eq!(guards.len(), 20);
    }

    #[test]
    fn a_waiter_gets_the_seat_when_it_frees_and_hears_what_it_waits_for() {
        let book = Arc::new(SeatBook::new());
        let first = book.try_claim(&local("m"), "s1").unwrap();
        let b2 = book.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let g = seat_of(b2.claim_waiting(&local("m"), "s2", Duration::from_millis(40), far(), &never, |o| {
                let _ = tx.send(o.what.clone());
            }));
            drop(g);
            let _ = done_tx.send(());
        });
        let first_note = rx.recv_timeout(Duration::from_secs(2)).expect("told at once that it waits");
        assert!(first_note.contains("s1"), "{first_note}");
        // Heartbeats keep coming while it waits.
        assert!(rx.recv_timeout(Duration::from_secs(2)).is_ok(), "no heartbeat while waiting");
        // A newcomer does not overtake the waiter.
        assert!(book.try_claim(&local("m"), "s-late").is_err());
        drop(first);
        done_rx.recv_timeout(Duration::from_secs(5)).expect("the waiter never got the freed seat");
        assert!(book.running().is_empty(), "the waiter ran and released the seat");
    }

    /// A newcomer never overtakes a job already waiting for the same seat,
    /// even in the instant the seat is free but the waiter has not woken.
    #[test]
    fn a_newcomer_never_overtakes_a_waiter_for_a_free_seat() {
        let book = Arc::new(SeatBook::new());
        book.book.lock().unwrap().waiting.push_back((local("m").key(), 999));
        let err = book.try_claim(&local("m"), "s-new").err().expect("the waiter holds its place");
        assert!(err.what.contains("waiting"), "{err:?}");
        assert!(book.try_claim(&local("other"), "s-other").is_ok(), "another seat's queue is not this one's");
    }

    /// A peek answers what a claim would, and never holds the seat: after
    /// it, a real claim still succeeds.
    #[test]
    fn a_peek_reports_a_claims_answer_without_taking_the_seat() {
        let book = Arc::new(SeatBook::new());
        assert_eq!(book.peek(&local("m")), None);
        assert!(book.try_claim(&local("m"), "s-real").is_ok(), "the peek held nothing");
        let held = book.try_claim(&local("n"), "s-held").unwrap();
        assert_eq!(book.peek(&local("n")), book.try_claim(&local("n"), "s-2").err(), "the same answer a claim gets");
        assert!(book.peek(&local("n")).is_some());
        drop(held);
        assert_eq!(book.peek(&local("n")), None);
        // A waiter ahead counts, as it does for a claim.
        book.book.lock().unwrap().waiting.push_back((local("w").key(), 0));
        assert!(book.peek(&local("w")).is_some());
    }

    /// A waiter takes a free seat only when it is first in line for it.
    #[test]
    fn a_waiter_behind_an_earlier_waiter_does_not_take_the_free_seat() {
        let book = Arc::new(SeatBook::new());
        book.book.lock().unwrap().waiting.push_back((local("m").key(), 0));
        book.book.lock().unwrap().next_ticket = 1;
        let b2 = book.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let g = seat_of(b2.claim_waiting(&local("m"), "s-second", Duration::from_millis(20), far(), &never, |_| {}));
            let _ = done_tx.send(());
            std::thread::sleep(Duration::from_millis(50));
            drop(g);
        });
        assert!(done_rx.recv_timeout(Duration::from_millis(200)).is_err(), "it jumped the earlier waiter");
        book.book.lock().unwrap().waiting.retain(|(_, t)| *t != 0);
        book.freed.notify_all();
        done_rx.recv_timeout(Duration::from_secs(5)).expect("it runs once it is first");
    }

    #[test]
    fn waiters_on_one_seat_are_served_in_arrival_order() {
        let book = Arc::new(SeatBook::new());
        let first = book.try_claim(&local("m"), "s0").unwrap();
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut handles = Vec::new();
        for i in 1..=3 {
            let (b, o) = (book.clone(), order.clone());
            let (tx, rx) = std::sync::mpsc::channel::<()>();
            handles.push(std::thread::spawn(move || {
                let g = seat_of(b.claim_waiting(&local("m"), &format!("s{i}"), Duration::from_secs(5), far(), &never, |_| {
                    let _ = tx.send(());
                }));
                o.lock().unwrap().push(i);
                std::thread::sleep(Duration::from_millis(10));
                drop(g);
            }));
            // Each waiter is queued before the next one arrives.
            rx.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        drop(first);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while order.lock().unwrap().len() < 3 {
            assert!(std::time::Instant::now() < deadline, "waiters never all ran: {:?}", order.lock().unwrap());
            std::thread::sleep(Duration::from_millis(10));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(*order.lock().unwrap(), vec![1, 2, 3]);
    }

    /// (#2916 stage 2 review M2) A waiter whose sender hung up leaves the
    /// queue without the seat, and the next waiter is not held behind it.
    #[test]
    fn a_cancelled_waiter_leaves_the_queue_and_never_takes_the_seat() {
        let book = Arc::new(SeatBook::new());
        let first = book.try_claim(&local("m"), "s1").unwrap();
        let gone = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (b2, g2) = (book.clone(), gone.clone());
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let cancelled = || g2.load(std::sync::atomic::Ordering::SeqCst);
            let w = b2.claim_waiting(&local("m"), "s2", Duration::from_millis(20), far(), &cancelled, |_| {});
            let _ = done_tx.send(matches!(w, Waited::Cancelled));
        });
        std::thread::sleep(Duration::from_millis(60));
        gone.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(done_rx.recv_timeout(Duration::from_secs(2)).unwrap(), "the wait ended as cancelled");
        assert!(book.book.lock().unwrap().waiting.is_empty(), "its ticket is gone");
        drop(first);
        assert!(book.try_claim(&local("m"), "s3").is_ok(), "nobody is left waiting for the seat");
    }

    /// (#2916 stage 2 review C1) A waiter past its deadline leaves the queue
    /// and says what held the seat.
    #[test]
    fn a_waiter_past_its_deadline_times_out_naming_what_held_the_seat() {
        let book = Arc::new(SeatBook::new());
        let _first = book.try_claim(&local("m"), "s1").unwrap();
        let b2 = book.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let w = b2.claim_waiting(&local("m"), "s2", Duration::from_secs(5), Instant::now() + Duration::from_millis(80), &never, |_| {});
            let _ = tx.send(match w {
                Waited::TimedOut(o) => Some(o.what),
                Waited::Seat(_) | Waited::Cancelled => None,
            });
        });
        // Well under the 5 s heartbeat: the deadline, not the heartbeat, ends it.
        let what = rx.recv_timeout(Duration::from_secs(2)).expect("the wait outlived its deadline");
        assert!(what.as_deref().is_some_and(|w| w.contains("s1")), "{what:?}");
        assert!(book.book.lock().unwrap().waiting.is_empty(), "its ticket is gone");
    }

    /// (5.0) Two claims under one session id (a retry, or a repeated sender
    /// session: the receiver's id is deterministic) are two seats. Dropping
    /// one frees one, so the cap still holds.
    #[test]
    fn two_claims_under_one_session_id_free_one_seat_at_a_time() {
        let book = Arc::new(SeatBook::new());
        let azure = endpoint("azure", Some(2));
        let a = book.try_claim(&azure, "s1").unwrap();
        let b = book.try_claim(&azure, "s1").unwrap();
        drop(a);
        let _s2 = book.try_claim(&azure, "s2").expect("one seat freed, so one fits");
        assert!(book.try_claim(&azure, "s3").is_err(), "3 running on a cap of 2");
        drop(b);
        assert_eq!(book.running().len(), 1, "only s2 is left");
    }

    #[test]
    fn dropping_both_claims_under_one_session_id_frees_both() {
        let book = Arc::new(SeatBook::new());
        let azure = endpoint("azure", Some(2));
        let a = book.try_claim(&azure, "s1").unwrap();
        let b = book.try_claim(&azure, "s1").unwrap();
        drop(a);
        drop(b);
        assert!(book.running().is_empty());
        assert!(book.try_claim(&azure, "s2").is_ok() && book.try_claim(&azure, "s3").is_ok());
    }

    /// (5.0) A local seat frees by its own ticket: a guard whose seat was
    /// already re-taken under the same session id never frees the new holder.
    #[test]
    fn a_local_guard_frees_only_its_own_claim() {
        let book = Arc::new(SeatBook::new());
        let a = book.try_claim(&local("m"), "s1").unwrap();
        // Simulate the seat being re-taken under the same session id.
        let ghost = {
            let mut b = book.book.lock().unwrap();
            b.local.remove("m");
            book.take(&mut b, &local("m"), "s1")
        };
        drop(a);
        assert_eq!(book.running(), vec!["s1".to_string()], "the old guard must not free the new claim");
        drop(ghost);
        assert!(book.running().is_empty());
    }

    /// (5.0) A refusal for an inline endpoint names its label, never its URL.
    #[test]
    fn the_busy_text_names_the_label_not_the_endpoint_key() {
        let book = Arc::new(SeatBook::new());
        let seat = WorkSeat::Unmanaged {
            endpoint: "https://user:hunter2@api.example.com/v1?key=sekret".into(),
            label: "an inline endpoint".into(),
            model: "gpt-x".into(),
            concurrent_calls: None,
        };
        let _a = book.try_claim(&seat, "h1").unwrap();
        let busy = book.try_claim(&seat, "h2").err().unwrap();
        assert!(busy.what.contains("an inline endpoint"), "{busy:?}");
        for leak in ["hunter2", "sekret", "example.com", "https"] {
            assert!(!busy.what.contains(leak), "{leak} leaked: {busy:?}");
        }
    }
}
