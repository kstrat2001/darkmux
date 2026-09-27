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
use std::time::Duration;

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
        let hosted_cap = if remote_concurrent_cap == 0 { usize::MAX } else { remote_concurrent_cap as usize };
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

    /// Wait for `seat`, first come first served, then take it. While
    /// waiting, `on_wait` is called with what holds the seat right away and
    /// again every `heartbeat`, so a waiting sender hears from this machine
    /// before its read deadline.
    pub fn claim_waiting(
        self: &Arc<Self>,
        seat: &WorkSeat,
        session: &str,
        heartbeat: Duration,
        mut on_wait: impl FnMut(&Occupied),
    ) -> SeatGuard {
        let key = seat.key();
        let mut b = self.book.lock().unwrap_or_else(|p| p.into_inner());
        let ticket = b.next_ticket;
        b.next_ticket += 1;
        b.waiting.push_back((key.clone(), ticket));
        let mut announced = false;
        loop {
            let first_for_key = b.waiting.iter().find(|(k, _)| *k == key).map(|(_, t)| *t) == Some(ticket);
            let held = self.occupied(&b, seat);
            if first_for_key && held.is_none() {
                b.waiting.retain(|(_, t)| *t != ticket);
                let guard = self.take(&mut b, seat, session);
                drop(b);
                // Wake the next waiter for another key that may now move.
                self.freed.notify_all();
                return guard;
            }
            let what = held.unwrap_or_else(|| Occupied { what: "earlier jobs are waiting for the same seat".into() });
            if !announced {
                announced = true;
                drop(b);
                on_wait(&what);
                b = self.book.lock().unwrap_or_else(|p| p.into_inner());
                continue;
            }
            let (next, timeout) = self.freed.wait_timeout(b, heartbeat).unwrap_or_else(|p| p.into_inner());
            b = next;
            if timeout.timed_out() {
                let first_for_key = b.waiting.iter().find(|(k, _)| *k == key).map(|(_, t)| *t) == Some(ticket);
                if !(first_for_key && self.occupied(&b, seat).is_none()) {
                    let what = self
                        .occupied(&b, seat)
                        .unwrap_or_else(|| Occupied { what: "earlier jobs are waiting for the same seat".into() });
                    drop(b);
                    on_wait(&what);
                    b = self.book.lock().unwrap_or_else(|p| p.into_inner());
                }
            }
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
            let g = b2.claim_waiting(&local("m"), "s2", Duration::from_millis(40), |o| {
                let _ = tx.send(o.what.clone());
            });
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
            let g = b2.claim_waiting(&local("m"), "s-second", Duration::from_millis(20), |_| {});
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
                let g = b.claim_waiting(&local("m"), &format!("s{i}"), Duration::from_secs(5), |_| {
                    let _ = tx.send(());
                });
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
}
