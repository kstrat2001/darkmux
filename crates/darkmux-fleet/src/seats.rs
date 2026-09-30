//! (#2916 stage 2) Which submitted jobs may run at once on a receiver.
//!
//! Busy is decided by what a job INVOKES, not by the machine as a whole:
//!
//! - a job on a **local** model holds that model for its whole run. LM Studio
//!   serves one request at a time per instance (measured 2026-09-26, #2914),
//!   so a second job for the same model is busy, while a job for a different
//!   local model is not;
//! - a job on a **hosted** endpoint runs beside other hosted jobs, up to this
//!   machine's `remote.concurrent_cap` (`0` = unbounded, the darkmux bound
//!   convention).
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
    /// A hosted endpoint this machine only sends requests to.
    Hosted { model: String },
}

impl WorkSeat {
    /// The key waiters queue on: one per local model, one shared by every
    /// hosted job.
    fn key(&self) -> String {
        match self {
            WorkSeat::Local { model } => format!("local:{model}"),
            WorkSeat::Hosted { .. } => "hosted".to_string(),
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

#[derive(Default)]
struct Book {
    /// Local model -> the session running on it.
    local: BTreeMap<String, String>,
    /// Sessions running on hosted endpoints.
    hosted: Vec<String>,
    /// Waiters in arrival order: (seat key, ticket).
    waiting: VecDeque<(String, u64)>,
    next_ticket: u64,
}

/// The receiver's record of running submitted jobs.
pub struct SeatBook {
    /// Hosted jobs allowed at once (`usize::MAX` for unbounded).
    hosted_cap: usize,
    book: Mutex<Book>,
    freed: Condvar,
}

/// What the book holds right now, for a machine card: the seats taken and how
/// many jobs wait, and nothing about who holds them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatSnapshot {
    /// The local models a submitted job holds, sorted.
    pub local_held: Vec<String>,
    /// Hosted jobs running.
    pub hosted_held: usize,
    /// Hosted jobs allowed at once; `None` is unbounded.
    pub hosted_cap: Option<usize>,
    /// Jobs waiting for a seat.
    pub waiting: usize,
}

/// Holds a seat; dropping it frees the seat and wakes the waiters.
pub struct SeatGuard {
    owner: Arc<SeatBook>,
    seat: WorkSeat,
    session: String,
}

impl Drop for SeatGuard {
    fn drop(&mut self) {
        let mut b = self.owner.book.lock().unwrap_or_else(|p| p.into_inner());
        match &self.seat {
            WorkSeat::Local { model } => {
                if b.local.get(model) == Some(&self.session) {
                    b.local.remove(model);
                }
            }
            WorkSeat::Hosted { .. } => {
                if let Some(i) = b.hosted.iter().position(|s| s == &self.session) {
                    b.hosted.remove(i);
                }
            }
        }
        drop(b);
        self.owner.freed.notify_all();
    }
}

impl SeatBook {
    /// `remote_concurrent_cap` is the receiver's `remote.concurrent_cap`;
    /// `0` means unbounded.
    pub fn new(remote_concurrent_cap: u32) -> Self {
        let hosted_cap = darkmux_types::config_access::jobs_at_once(remote_concurrent_cap as usize);
        Self { hosted_cap, book: Mutex::new(Book::default()), freed: Condvar::new() }
    }

    /// Why `seat` cannot be taken now, or `None` when it can.
    fn occupied(&self, b: &Book, seat: &WorkSeat) -> Option<Occupied> {
        match seat {
            WorkSeat::Local { model } => b.local.get(model).map(|s| Occupied { what: format!("{s} is running on {model}") }),
            WorkSeat::Hosted { .. } if b.hosted.len() >= self.hosted_cap => Some(Occupied {
                what: format!(
                    "{} hosted job(s) are running, its `remote.concurrent_cap` ({})",
                    b.hosted.len(),
                    b.hosted.join(", ")
                ),
            }),
            WorkSeat::Hosted { .. } => None,
        }
    }

    fn take(self: &Arc<Self>, b: &mut Book, seat: &WorkSeat, session: &str) -> SeatGuard {
        match seat {
            WorkSeat::Local { model } => {
                b.local.insert(model.clone(), session.to_string());
            }
            WorkSeat::Hosted { .. } => b.hosted.push(session.to_string()),
        }
        SeatGuard { owner: Arc::clone(self), seat: seat.clone(), session: session.to_string() }
    }

    /// Take `seat` now, or say what holds it. A job already waiting for the
    /// same seat counts as holding it: a newcomer never overtakes the queue.
    pub fn try_claim(self: &Arc<Self>, seat: &WorkSeat, session: &str) -> Result<SeatGuard, Occupied> {
        let mut b = self.book.lock().unwrap_or_else(|p| p.into_inner());
        let key = seat.key();
        if let Some(o) = self.occupied(&b, seat) {
            return Err(o);
        }
        if b.waiting.iter().any(|(k, _)| *k == key) {
            return Err(Occupied { what: "earlier jobs are waiting for the same seat".into() });
        }
        Ok(self.take(&mut b, seat, session))
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
            hosted_held: b.hosted.len(),
            hosted_cap: (self.hosted_cap != usize::MAX).then_some(self.hosted_cap),
            waiting: b.waiting.len(),
        }
    }

    /// Sessions running now (for tests and logs).
    pub fn running(&self) -> Vec<String> {
        let b = self.book.lock().unwrap_or_else(|p| p.into_inner());
        b.local.values().cloned().chain(b.hosted.iter().cloned()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(m: &str) -> WorkSeat {
        WorkSeat::Local { model: m.into() }
    }
    fn hosted() -> WorkSeat {
        WorkSeat::Hosted { model: "gpt-x".into() }
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
    fn the_snapshot_names_held_seats_and_the_cap_and_no_sessions() {
        let book = Arc::new(SeatBook::new(2));
        let empty = book.snapshot();
        assert_eq!(empty, SeatSnapshot { local_held: vec![], hosted_held: 0, hosted_cap: Some(2), waiting: 0 });
        let _a = book.try_claim(&local("qwen-35b"), "s1").expect("free");
        let _h = book.try_claim(&hosted(), "s2").expect("free");
        let snap = book.snapshot();
        assert_eq!(snap.local_held, vec!["qwen-35b".to_string()]);
        assert_eq!(snap.hosted_held, 1);
        assert!(!format!("{snap:?}").contains("s1"), "a session id must not reach the snapshot");
        drop(_a);
        assert!(book.snapshot().local_held.is_empty(), "a freed seat leaves the snapshot");
        assert_eq!(SeatBook::new(0).snapshot().hosted_cap, None, "0 means unbounded");
    }

    #[test]
    fn one_job_per_local_model_and_different_models_run_together() {
        let book = Arc::new(SeatBook::new(1));
        let a = book.try_claim(&local("qwen-35b"), "s1").expect("free");
        let busy = book.try_claim(&local("qwen-35b"), "s2").err().expect("same model is busy");
        assert!(busy.what.contains("s1") && busy.what.contains("qwen-35b"), "names what is running: {busy:?}");
        let _b = book.try_claim(&local("qwen-4b"), "s3").expect("a different local model is not busy");
        drop(a);
        assert!(book.try_claim(&local("qwen-35b"), "s4").is_ok(), "the seat frees when the job ends");
    }

    #[test]
    fn hosted_jobs_run_together_up_to_the_cap() {
        let book = Arc::new(SeatBook::new(2));
        let _a = book.try_claim(&hosted(), "h1").unwrap();
        let _b = book.try_claim(&hosted(), "h2").expect("second hosted job fits the cap");
        let busy = book.try_claim(&hosted(), "h3").err().expect("past the cap");
        assert!(busy.what.contains("remote.concurrent_cap") && busy.what.contains("h1, h2"), "{busy:?}");
        // A hosted job never blocks a local one, and the reverse.
        assert!(book.try_claim(&local("m"), "l1").is_ok());
    }

    #[test]
    fn a_zero_cap_means_unbounded_hosted_jobs() {
        let book = Arc::new(SeatBook::new(0));
        let guards: Vec<_> = (0..20).map(|i| book.try_claim(&hosted(), &format!("h{i}")).unwrap()).collect();
        assert_eq!(guards.len(), 20);
    }

    #[test]
    fn a_waiter_gets_the_seat_when_it_frees_and_hears_what_it_waits_for() {
        let book = Arc::new(SeatBook::new(1));
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
        let book = Arc::new(SeatBook::new(1));
        book.book.lock().unwrap().waiting.push_back((local("m").key(), 999));
        let err = book.try_claim(&local("m"), "s-new").err().expect("the waiter holds its place");
        assert!(err.what.contains("waiting"), "{err:?}");
        assert!(book.try_claim(&local("other"), "s-other").is_ok(), "another seat's queue is not this one's");
    }

    /// A waiter takes a free seat only when it is first in line for it.
    #[test]
    fn a_waiter_behind_an_earlier_waiter_does_not_take_the_free_seat() {
        let book = Arc::new(SeatBook::new(1));
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
        let book = Arc::new(SeatBook::new(1));
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
        let book = Arc::new(SeatBook::new(1));
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
        let book = Arc::new(SeatBook::new(1));
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
}
