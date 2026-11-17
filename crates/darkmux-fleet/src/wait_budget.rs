//! How long a waited-on job's connection lives, and what it must hold.
//!
//! A sender that waits for a job's answer holds one connection to the
//! receiver while the job queues for its seat and while it runs; the
//! receiver ends every connection at its lifetime. The sender's read
//! deadline ([`reply_wait`]) and the receiver's connection lifetime and
//! queue window ([`WaitBudget`]) all come from here, so a receiver never
//! ends a connection whose sender is still owed the answer (#3429).

use crate::job::{WorkJob, MAX_WORK_TIMEOUT_SECONDS};
use std::time::Duration;

/// (#3429) What a waited-on job's answer gets past the job's own run: the
/// receiver's bookkeeping and the answer's trip back.
pub const ANSWER_SLACK: Duration = Duration::from_secs(120);

/// (#3429) The longest a target may spend, before an agentic job's work
/// begins, getting what its sandbox needs on first use: a Docker target
/// pulls the runtime image (bounded by `darkmux_crew::runtime_image::PULL_TIMEOUT`),
/// and a sandbox on the host fetches its managed toolchain (bounded by
/// `darkmux_crew::managed_toolchain::FIRST_USE_BOUND`). The sender cannot
/// see which sandbox the target runs, so it allows the longer.
pub const SANDBOX_FIRST_USE: Duration = longer(
    darkmux_crew::runtime_image::PULL_TIMEOUT,
    darkmux_crew::managed_toolchain::FIRST_USE_BOUND,
);

const fn longer(a: Duration, b: Duration) -> Duration {
    if a.as_nanos() >= b.as_nanos() {
        a
    } else {
        b
    }
}

/// The least time any waited-on job may wait for its seat, a job at the
/// timeout cap included.
pub const QUEUE_ROOM: Duration = Duration::from_secs(300);

/// The longest a receiver keeps one connection open: a job at the timeout
/// cap, its sandbox's first-use setup and its answer's slack, after
/// [`QUEUE_ROOM`] waiting for its seat.
pub const CONNECTION_LIFETIME: Duration = Duration::from_secs(MAX_WORK_TIMEOUT_SECONDS as u64)
    .saturating_add(SANDBOX_FIRST_USE)
    .saturating_add(ANSWER_SLACK)
    .saturating_add(QUEUE_ROOM);

/// What one waited-on job's connection must hold, and how long it lives.
/// The receiver reads its connection lifetime and queue window from it, the
/// sender its read deadline; tests shorten every bound together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitBudget {
    /// An agentic job's first-use sandbox setup ([`SANDBOX_FIRST_USE`]).
    pub first_use: Duration,
    /// The answer's slack after the run ([`ANSWER_SLACK`]).
    pub answer_slack: Duration,
    /// How long the receiver keeps one connection open ([`CONNECTION_LIFETIME`]).
    pub connection_lifetime: Duration,
}

impl WaitBudget {
    pub const PRODUCTION: WaitBudget = WaitBudget {
        first_use: SANDBOX_FIRST_USE,
        answer_slack: ANSWER_SLACK,
        connection_lifetime: CONNECTION_LIFETIME,
    };

    /// What a waited-on job needs of its connection once it has its seat:
    /// its timeout, first the sandbox's first-use setup for an agentic job
    /// (a single-shot job starts no sandbox), then the answer's slack.
    pub fn run_needs(&self, job: &WorkJob) -> Duration {
        self.overhead(job) + Duration::from_secs(u64::from(job.timeout_seconds))
    }

    /// What a waited-on job needs past its own timeout.
    fn overhead(&self, job: &WorkJob) -> Duration {
        let first_use = if job.single_shot.is_some() {
            Duration::ZERO
        } else {
            self.first_use
        };
        first_use + self.answer_slack
    }

    /// How long a waited-on job may wait for its seat: what its connection
    /// has left after the job's run needs; zero when nothing is left.
    pub fn queue_room(&self, job: &WorkJob) -> Duration {
        self.connection_lifetime.saturating_sub(self.run_needs(job))
    }

    /// Whether a waited-on job's run fits one connection at all; `Err` names
    /// the bound and the longest timeout that fits.
    pub fn fits(&self, job: &WorkJob) -> Result<(), String> {
        let needs = self.run_needs(job);
        if needs <= self.connection_lifetime {
            return Ok(());
        }
        let longest = self
            .connection_lifetime
            .saturating_sub(self.overhead(job))
            .as_secs();
        Err(format!(
            "a waited-on job with a {}s timeout needs {needs:?} of one connection (its timeout, then \
             {:?} for the sandbox's first-use setup and {:?} for the answer), and a connection here \
             lives at most {:?}; send it with a timeout of at most {longest}s, or without waiting",
            job.timeout_seconds,
            needs - self.answer_slack - Duration::from_secs(u64::from(job.timeout_seconds)),
            self.answer_slack,
            self.connection_lifetime,
        ))
    }
}

/// (#3429) How long a sender waits for the next line of a job's reply (a
/// read deadline, so each `queued` line restarts it). An unwaited job's one
/// line is the acknowledgment, sent before anything runs. A waited-on job's
/// last line comes after its run: [`WaitBudget::run_needs`], the same bound
/// the receiver leaves room for in the connection. The job's timeout bounds
/// only a single-shot job's run: an agentic job runs on the receiver's own
/// inactivity budget (`timeout_override_seconds` does not cross the wire),
/// so it can outlast this wait, and the sender then reports the answer
/// lost, naming the session to follow.
pub fn reply_wait(job: &WorkJob, wait: bool) -> Duration {
    if !wait {
        return ACKNOWLEDGMENT_WAIT;
    }
    WaitBudget::PRODUCTION.run_needs(job)
}

/// How long a sender waits for an unwaited job's acknowledgment.
const ACKNOWLEDGMENT_WAIT: Duration = Duration::from_secs(60);

#[cfg(test)]
mod tests {
    use super::*;

    fn job(timeout_seconds: u32, single_shot: bool) -> WorkJob {
        let mut j = crate::build_work_job(
            "studio".into(),
            "coder".into(),
            "hi".into(),
            crate::test_session("s"),
            None,
            None,
            None,
            timeout_seconds,
            None,
        );
        j.single_shot = single_shot.then_some(crate::SingleShotJob {
            humor: 0,
            surface: darkmux_flow::payload::RadioSurface::Cli,
            max_completion_tokens: 512,
        });
        j
    }

    /// Every timeout up to the cap fits one connection with [`QUEUE_ROOM`]
    /// left to queue, agentic or single-shot.
    #[test]
    fn every_timeout_up_to_the_cap_fits_with_room_to_queue() {
        for single_shot in [false, true] {
            for t in [1, 60, 300, 600, 1800, MAX_WORK_TIMEOUT_SECONDS] {
                let j = job(t, single_shot);
                assert_eq!(WaitBudget::PRODUCTION.fits(&j), Ok(()), "--timeout {t}");
                assert!(
                    WaitBudget::PRODUCTION.queue_room(&j) >= QUEUE_ROOM,
                    "--timeout {t}, single-shot {single_shot}"
                );
            }
        }
    }

    /// The queue window and the run's needs fill the connection exactly:
    /// a job that waits its whole window still has its run's needs left.
    #[test]
    fn the_queue_window_leaves_the_run_its_needs() {
        let j = job(300, false);
        let b = WaitBudget::PRODUCTION;
        assert_eq!(b.queue_room(&j) + b.run_needs(&j), CONNECTION_LIFETIME);
        assert_eq!(
            b.run_needs(&j),
            Duration::from_secs(300) + SANDBOX_FIRST_USE + ANSWER_SLACK
        );
        assert_eq!(
            reply_wait(&j, true),
            b.run_needs(&j),
            "the sender waits what the receiver leaves room for"
        );
    }

    /// A job whose run does not fit is refused naming the bound and the
    /// longest timeout that does.
    #[test]
    fn a_job_that_cannot_fit_names_the_bound() {
        let b = WaitBudget {
            first_use: Duration::from_secs(10),
            answer_slack: Duration::from_secs(5),
            connection_lifetime: Duration::from_secs(60),
        };
        assert_eq!(
            b.fits(&job(45, false)),
            Ok(()),
            "45 + 10 + 5 is exactly the lifetime"
        );
        let why = b.fits(&job(46, false)).unwrap_err();
        assert!(why.contains("46s timeout needs 61s"), "{why}");
        assert!(
            why.contains("10s for the sandbox's first-use setup and 5s for the answer"),
            "{why}"
        );
        assert!(why.contains("lives at most 60s"), "{why}");
        assert!(why.contains("at most 45s"), "{why}");
        assert_eq!(
            b.fits(&job(55, true)),
            Ok(()),
            "a single-shot job sets up no sandbox"
        );
    }
}
