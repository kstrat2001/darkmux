//! (#2774 round-4, scope narrowed round-6) Validated thermal SEVERITY bands
//! — the value that makes an unsatisfiable or tautological thermal tier
//! unrepresentable *in its severity comparisons*. That is one instance of a
//! wider rule, stated next; the rest of the rule lives in
//! [`crate::thermal_governor`] and is covered by tests rather than by types.
//!
//! ## The rule, stated first
//!
//! **A threshold comparison must not degenerate at its knob's end value.**
//!
//! That is the shape six review rounds on #2774 kept finding. It is a rule
//! about COMPARISONS, not about severities — a fact this module's own doc
//! got wrong for one round, which is why the rule now leads.
//!
//! What "degenerate" means, in both directions:
//!
//! - **unsatisfiable** at some end value — no input can satisfy the
//!   predicate, so the behavior it gates is dead;
//! - **tautological** at some end value — every input satisfies it, so the
//!   complement — the "when do we leave" half — is dead instead.
//!
//! Either way a control-flow edge silently disappears at one setting of one
//! knob, while reading correctly at every value anyone tested.
//!
//! ## Where it has appeared, and what covers each
//!
//! The rule has two instances in this module's subject matter. This file
//! ends the first one STRUCTURALLY; the second is a code rule with a test
//! sweep behind it, and is named here so a reader takes away the rule
//! rather than a settled claim.
//!
//! ### Instance 1 — SEVERITY comparisons (what this module makes unrepresentable)
//!
//! Found three times, each in a different predicate, each introduced by the
//! previous round's fix:
//!
//! | round | the predicate | the shape |
//! |---|---|---|
//! | 2 | recovery gained "and strictly milder than `pause_at`", which for `pause_at = "nominal"` read `sev < 0` on a `usize` | **unsatisfiable** — recovery unreachable, so a machine reading `nominal` for 15 minutes pause-accumulated into a false `thermal-critical` breaker trip plus a crawl `STOP` file |
//! | 3 | arming became `severity(pause_at) > severity(resume_at)` | a PAIR relationship that never checks either value against the enum's ENDS — so it caught the round-2 shape and nothing else |
//! | 4 | tier 2's duty band `sev >= severity(resume_at)`, which for `resume_at = "nominal"` read `sev >= 0` | **tautological** — `!in_duty_band` unreachable, so `DutyCycle` could never be exited and a cold machine picked up a permanent, ratcheting turn delay |
//!
//! The local cause was **severity comparisons written ad hoc against raw
//! `usize` ranks, with nothing guaranteeing that the resulting band is
//! inhabited or that its complement is reachable.** Each fix patched one
//! predicate and left the shape standing for the next round to fall into,
//! which is what "What replaces instance 1" below is a response to.
//!
//! ### Instance 2 — TIME and COUNT comparisons (round 6, `thermal_governor`)
//!
//! The same shape on the module's `accumulator >= knob` comparisons, where
//! the end value is `0`:
//!
//! | knob | the predicate | the shape |
//! |---|---|---|
//! | `resume_hold_ms = 0` | `resume_hold_accum_ms >= resume_hold_ms` | **tautological** — the accumulator stood in for "a recovery reading was seen", an implication that holds only while the knob is positive. A machine reading `serious` every sample resumed at FULL SPEED on the tick after it paused, and the phantom recoveries minted fresh episodes until tier 4's terminal operator-gated hold, three samples in |
//! | `max_pause_ms = 0` | `pause_episode_ms >= max_pause_ms` | **tautological** — a `thermal-critical` breaker plus `STOP` file on the sample after the pause, on a machine that had never reported `critical`, for an operator whose `0` meant "rest as long as it takes" — darkmux's standing reading of a `0` bound, per the repo's own CLAUDE.md |
//!
//! Neither has a band to refuse, so the structural move this file makes
//! does not apply. What covers them instead:
//!
//! - **the fix is the PREDICATE, not a floor on the knob.** `resume_hold_ms`
//!   is now gated on `is_recovery_reading(reading) && accum >= hold` — the
//!   shape the duty-cycle branch already used (`in_duty_band && …`). A
//!   floor would have been correct at `0` and left the tautology one edit
//!   away; a predicate is correct at every value. `max_pause_ms` reads its
//!   `0` as UNBOUNDED, the meaning every other darkmux bound gives it.
//! - **the sweep is the test.** `thermal_governor`'s
//!   `no_knob_boundary_value_manufactures_a_transition_the_readings_never_justified`
//!   enumerates every knob at its boundary values and asserts two
//!   invariants that follow from the READINGS alone — a machine that never
//!   read a recovery state never resumes; a cold machine never pauses. A
//!   knob value cannot manufacture a transition the hardware did not
//!   justify, which is what a degenerate comparison does.
//!
//! **When you add a knob to this module, go to that sweep first.** The
//! question to answer is not "is my comparison right" — it read right in
//! all six rounds — but "at each END of this knob's range, is the
//! comparison still satisfiable AND still falsifiable."
//!
//! ## What replaces instance 1
//!
//! A band is a VALUE, built once, by a constructor that can refuse — and
//! the two degenerate shapes are exactly its two refusal conditions:
//!
//! - **unsatisfiable**: no reading in the band's universe satisfies it;
//! - **tautological**: every reading in that universe satisfies it, so the
//!   complement — the "when do we leave" half — is unreachable.
//!
//! [`Band::new`] is the ONLY constructor and returns `Option`, so a band
//! that exists is a band with at least one reading inside it and at least
//! one outside it. Both witnesses are retrievable ([`Band::a_member`],
//! [`Band::a_non_member`]) — the guarantee is constructive, not a comment.
//!
//! Tier logic then reads its band off [`ThermalBands`] instead of
//! open-coding a rank comparison. A tier whose band was refused is
//! `None` — disarmed, with a stated reason ([`ThermalBands::disarm_notes`])
//! that `darkmux doctor` and the dispatch-start warning BOTH render, so the
//! two surfaces cannot disagree about what the ladder will do.
//!
//! ## The universe is smaller than the enum, and that is the point
//!
//! `THERMAL_STATES` has four entries, but the soft tiers never see all
//! four: `on_sample` evaluates the BREAKER first, and `critical` — along
//! with any state name this build does not recognize — trips it and returns
//! before a single band is consulted. So the reading universe for every
//! soft-tier band is the known states STRICTLY MILDER than `critical`, and
//! that is what [`SoftReading`] denotes.
//!
//! This distinction is load-bearing, not pedantry: round 4's defect is
//! invisible unless you measure against this universe. With
//! `resume_at = nominal` and `pause_at = critical`, the duty band
//! `nominal..=critical` looks like a proper subset of a four-value enum —
//! it excludes nothing only once you notice `critical` can never arrive
//! here.

use crate::host_probe::thermal::THERMAL_STATES;

/// Rank of the state the BREAKER owns (`critical`, the last and most severe
/// entry in [`THERMAL_STATES`]). Derived rather than hardcoded;
/// `the_breaker_rank_is_critical` pins that the derivation still names the
/// state the breaker actually checks for.
const BREAKER_RANK: u8 = (THERMAL_STATES.len() - 1) as u8;

/// The most severe rank a SOFT tier can ever be asked about: one below the
/// breaker's. Every band in this module has this as its universe ceiling
/// unless a narrower one is passed.
const SOFT_MAX: u8 = BREAKER_RANK - 1;

/// Rank of a state name this build recognizes. `None` for anything else.
fn rank_of_known(state: &str) -> Option<u8> {
    THERMAL_STATES.iter().position(|s| *s == state).map(|p| p as u8)
}

/// A thermal reading that has already passed the breaker check: a state
/// name this build recognizes AND strictly milder than `critical`.
///
/// This type IS the soft tiers' reading universe. A `SoftReading` cannot be
/// constructed from `critical` or from an unrecognized name ([`Self::of`]
/// returns `None`), which is what keeps a band's inhabitation claim honest:
/// "at least one reading satisfies this" means at least one reading that
/// can actually reach the predicate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SoftReading(u8);

impl SoftReading {
    /// `None` when `state` is `critical`, or a name this build does not
    /// know — both of which the breaker has already claimed by the time any
    /// soft tier runs. Callers use that `None` as the breaker signal rather
    /// than re-deriving one (`thermal_governor::on_sample` does exactly
    /// that), so the two definitions of "breaker-class reading" cannot
    /// drift apart.
    pub fn of(state: &str) -> Option<Self> {
        match rank_of_known(state) {
            Some(r) if r <= SOFT_MAX => Some(Self(r)),
            _ => None,
        }
    }

    /// Every reading a soft tier can ever be handed, in severity order.
    /// The enumeration the class-regression test sweeps.
    pub fn all() -> impl Iterator<Item = Self> {
        (0..=SOFT_MAX).map(Self)
    }

    /// The state name this reading came from.
    pub fn name(self) -> &'static str {
        THERMAL_STATES[self.0 as usize]
    }
}

/// A contiguous band of soft readings that is guaranteed INHABITED and
/// PROPER: at least one reading is inside it, and at least one reading of
/// its universe is outside it.
///
/// There is no other constructor, no public field, and no way to widen one
/// after the fact — so "this band has an unreachable complement" is not a
/// state a `Band` can be in, and the predicate that decides a tier's
/// behavior cannot be a tautology or a contradiction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Band {
    lo: u8,
    hi: u8,
    /// The most severe reading this band is ever tested against. Usually
    /// [`SOFT_MAX`]; narrower when an earlier branch has already claimed
    /// the top of the range (tier 2's band is only ever evaluated on
    /// readings strictly below `pause_at`, because `pause_at` and above
    /// returned into tier 3 already).
    universe_hi: u8,
}

impl Band {
    /// The only constructor. `None` — the tier is disarmed — when the band
    /// is either of the two shapes this module exists to make
    /// unrepresentable:
    ///
    /// - **unsatisfiable** (`lo` above the universe's ceiling): no reading
    ///   can ever be in it, so the behavior it gates is dead code;
    /// - **tautological** (spanning the whole universe): no reading can
    ///   ever be OUT of it, so whatever the complement gates — leaving a
    ///   state, resuming, exiting a duty cycle — is unreachable.
    ///
    /// `hi` is clamped to `universe_hi` before the tests, so passing a `hi`
    /// past the ceiling describes the same band as passing the ceiling; it
    /// is the REACHABLE extent that decides, never the written one.
    fn new(lo: u8, hi: u8, universe_hi: u8) -> Option<Self> {
        let hi = hi.min(universe_hi);
        if lo > hi {
            return None; // unsatisfiable
        }
        if lo == 0 && hi == universe_hi {
            return None; // tautological — the complement is unreachable
        }
        Some(Self { lo, hi, universe_hi })
    }

    /// Whether this band covers `reading`. The ONE way tier logic asks the
    /// question.
    pub fn contains(self, reading: SoftReading) -> bool {
        reading.0 >= self.lo && reading.0 <= self.hi
    }

    /// A reading inside this band. Exists by construction — this is the
    /// inhabitation guarantee, returned rather than asserted.
    pub fn a_member(self) -> SoftReading {
        SoftReading(self.lo)
    }

    /// A reading of this band's universe that is OUTSIDE it. Exists by
    /// construction — this is the reachable-complement guarantee. Below the
    /// band when there is room below, above it otherwise.
    pub fn a_non_member(self) -> SoftReading {
        if self.lo > 0 {
            SoftReading(self.lo - 1)
        } else {
            SoftReading(self.hi + 1)
        }
    }
}

/// Tiers 3 and 4's two bands, which are only coherent as a PAIR.
///
/// Invariant, enforced by the only constructor: `entry` and `recovery` are
/// DISJOINT. An overlap is round 1's defect — one unchanging reading that
/// is at once "hot enough to pause" and "cool enough to resume", so the
/// governor cycles, manufacturing a fresh `serious` EPISODE every
/// `resume_hold_ms` and reaching tier 4's terminal operator-gated hold in
/// about a minute on a machine that never got hot.
///
/// (#2774 round-6 C3) The fields are PRIVATE, for the same reason
/// [`Band`]'s are: `pub` fields make the struct literal a second
/// constructor, and `PauseBands { entry, recovery }` from any two `Band`s
/// bypasses [`PauseBands::new`] and the disjointness check above. Nothing
/// did that — but "the constructor is the only way in" is the whole
/// mechanism, and a `pub` field leaves it true only by convention.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PauseBands {
    entry: Band,
    recovery: Band,
}

impl PauseBands {
    fn new(entry: Band, recovery: Band) -> Option<Self> {
        // Disjoint: nothing in `entry` may also be in `recovery`. Cheap to
        // check exhaustively — the universe has three values.
        if SoftReading::all().any(|r| entry.contains(r) && recovery.contains(r)) {
            return None;
        }
        Some(Self { entry, recovery })
    }

    /// At or above `pause_at`: enter tier 3's pause (or, at the episode
    /// threshold, tier 4's hold).
    pub fn entry(self) -> Band {
        self.entry
    }

    /// At or below `resume_at`: a reading that counts toward clearing an
    /// active pause.
    pub fn recovery(self) -> Band {
        self.recovery
    }
}

/// Why one or more tiers will not run, in a form both `darkmux doctor` and
/// the dispatch-start warning render — so the two surfaces cannot disagree
/// about what the ladder is about to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisarmNote {
    /// Which tiers this silences, for the operator ("tiers 2, 3 and 4").
    pub tiers: &'static str,
    /// What is wrong, in terms of the operator's own config values.
    pub why: String,
    /// The `darkmux config set …` line that fixes it.
    pub remedy: String,
}

/// Every soft tier's band, resolved ONCE from the configured
/// `pause_at`/`resume_at` pair.
///
/// A tier is armed iff its band survived [`Band::new`]. Tier logic reads
/// these; it never compares severity ranks itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThermalBands {
    duty: Option<Band>,
    pause: Option<PauseBands>,
    notes: Vec<DisarmNote>,
}

impl ThermalBands {
    /// Resolve the bands for a configured threshold pair.
    ///
    /// Two whole-ladder refusals come first, because neither leaves an
    /// interpretable config to build tier bands from:
    ///
    /// 1. **An unrecognized token in either slot.** There is no honest rank
    ///    for a name this build does not know, and guessing one is how
    ///    round 3's `unwrap_or(THERMAL_STATES.len())` armed a typo'd
    ///    `pause_at = "seroius"` at rank 4 — arming the ladder while tiers
    ///    3 and 4 were unreachable, and silencing the very warning that
    ///    would have said so.
    /// 2. **`pause_at` not strictly more severe than `resume_at`.** Every
    ///    reading is then both "hot enough to pause" and "cool enough to
    ///    resume"; there is no tie-break that is not the ladder acting on
    ///    evidence it does not have. (`PauseBands`' disjointness invariant
    ///    would refuse tier 3 for this pair on its own; the explicit check
    ///    is what extends the refusal to tier 2, whose band can look
    ///    perfectly well-formed under an inverted pair.)
    ///
    /// Past those, each tier gets its own band and can be refused alone.
    pub fn resolve(pause_at: &str, resume_at: &str) -> Self {
        let valid = THERMAL_STATES.join("|");
        let (Some(p), Some(r)) = (rank_of_known(pause_at), rank_of_known(resume_at)) else {
            let mut bad = Vec::new();
            if rank_of_known(pause_at).is_none() {
                bad.push(format!("pause_at=`{pause_at}`"));
            }
            if rank_of_known(resume_at).is_none() {
                bad.push(format!("resume_at=`{resume_at}`"));
            }
            return Self::disarmed(DisarmNote {
                tiers: "tiers 2, 3 and 4",
                why: format!(
                    "unrecognized thermal state: {} — valid: {valid}. A name this build does \
                     not know has no severity rank, so no band can be built from it: no \
                     duty-cycle, no pause/resume, no episode-count hold.",
                    bad.join(", ")
                ),
                remedy: format!("darkmux config set runtime.thermal.pause_at <{valid}>"),
            });
        };
        if p <= r {
            return Self::disarmed(DisarmNote {
                tiers: "tiers 2, 3 and 4",
                why: format!(
                    "pause_at=`{pause_at}` is not strictly more severe than \
                     resume_at=`{resume_at}` — the ladder needs a real band between them for \
                     the entry/resume holds to occupy, so with these equal (or inverted) every \
                     reading would be both hot enough to pause and cool enough to resume at \
                     once: no duty-cycle, no pause/resume, no episode-count hold."
                ),
                remedy: format!(
                    "darkmux config set runtime.thermal.resume_at <a state milder than \
                     {pause_at}> (valid: {valid})"
                ),
            });
        }

        let mut notes = Vec::new();

        // Tiers 3/4. Entry is `pause_at`..=the top of the soft universe;
        // recovery is the bottom..=`resume_at`.
        let pause = Band::new(p, SOFT_MAX, SOFT_MAX)
            .zip(Band::new(0, r, SOFT_MAX))
            .and_then(|(entry, recovery)| PauseBands::new(entry, recovery));
        if pause.is_none() {
            notes.push(DisarmNote {
                tiers: "tiers 3 and 4",
                why: format!(
                    "pause_at=`{pause_at}` is the breaker's own threshold, so no soft-tier \
                     reading can ever reach it: a `{}` reading trips the breaker (pause + a \
                     crawl STOP file) before the pause/resume ladder is consulted.",
                    THERMAL_STATES[BREAKER_RANK as usize]
                ),
                remedy: format!(
                    "darkmux config set runtime.thermal.pause_at {}",
                    THERMAL_STATES[SOFT_MAX as usize]
                ),
            });
        }

        // Tier 2. Its band is `resume_at`..=whatever is left BELOW the
        // pause entry — tier 3's branch returns first, so the readings this
        // band is tested against stop one short of `pause_at`. When tier 3
        // is disarmed nothing returns first, and the ceiling is the soft
        // universe's own.
        let duty_ceiling = pause.map_or(SOFT_MAX, |b| b.entry().a_non_member().0);
        let duty = Band::new(r, duty_ceiling, duty_ceiling);
        if duty.is_none() {
            notes.push(DisarmNote {
                tiers: "tier 2",
                why: format!(
                    "resume_at=`{resume_at}` is the mildest state there is, so every reading \
                     below pause_at=`{pause_at}` is inside the duty-cycle band and none is \
                     outside it — the governor could enter the duty cycle and never leave, \
                     pacing every turn of the run on a machine that is not hot.",
                ),
                remedy: format!(
                    "darkmux config set runtime.thermal.resume_at <a state between \
                     {} and {pause_at}> (valid: {valid})",
                    THERMAL_STATES[0]
                ),
            });
        }

        Self { duty, pause, notes }
    }

    fn disarmed(note: DisarmNote) -> Self {
        Self { duty: None, pause: None, notes: vec![note] }
    }

    /// Tier 2's duty band. `None` = tier 2 disarmed.
    pub fn duty(&self) -> Option<Band> {
        self.duty
    }

    /// Tiers 3/4's entry+recovery pair. `None` = both disarmed.
    pub fn pause(&self) -> Option<PauseBands> {
        self.pause
    }

    /// Whether ANY soft tier runs on this config.
    pub fn any_armed(&self) -> bool {
        self.duty.is_some() || self.pause.is_some()
    }

    /// Why whatever is disarmed is disarmed — empty when the whole ladder
    /// is armed.
    pub fn disarm_notes(&self) -> &[DisarmNote] {
        &self.notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `SOFT_MAX`/`BREAKER_RANK` are derived from the array's LENGTH, so a
    /// future reorder of `THERMAL_STATES` would silently move them. This
    /// pins that the derived breaker rank still names the state
    /// `thermal_governor`'s breaker actually checks for.
    #[test]
    fn the_breaker_rank_is_critical() {
        assert_eq!(THERMAL_STATES[BREAKER_RANK as usize], "critical");
        assert_eq!(THERMAL_STATES[SOFT_MAX as usize], "serious");
    }

    #[test]
    fn a_soft_reading_refuses_breaker_class_names() {
        assert_eq!(SoftReading::of("nominal").map(|r| r.name()), Some("nominal"));
        assert_eq!(SoftReading::of("serious").map(|r| r.name()), Some("serious"));
        assert_eq!(SoftReading::of("critical"), None, "critical is the breaker's");
        assert_eq!(SoftReading::of("unknown-9"), None, "so is a name we don't know");
        assert_eq!(SoftReading::of("Serious"), None, "…and an unnormalized one");
        assert_eq!(SoftReading::all().count(), 3);
    }

    /// The constructor's whole job, stated as three cases.
    #[test]
    fn band_new_refuses_the_two_unrepresentable_shapes() {
        // Tautological: spans the universe, complement unreachable.
        assert_eq!(Band::new(0, SOFT_MAX, SOFT_MAX), None);
        // …including when `hi` is written PAST the ceiling, which describes
        // the same band. This is round 4's exact shape: `sev >= nominal`
        // with no upper bound at all.
        assert_eq!(Band::new(0, 99, SOFT_MAX), None);
        // Unsatisfiable: `lo` past the ceiling. This is `pause_at =
        // critical` — nothing a soft tier sees can be in it.
        assert_eq!(Band::new(BREAKER_RANK, SOFT_MAX, SOFT_MAX), None);
        // Proper: inhabited, and something is outside it.
        let b = Band::new(1, SOFT_MAX, SOFT_MAX).expect("fair..=serious is a proper band");
        assert!(b.contains(SoftReading::of("fair").unwrap()));
        assert!(!b.contains(SoftReading::of("nominal").unwrap()));
    }

    /// `PauseBands`' own invariant, exercised DIRECTLY rather than only
    /// through `resolve`.
    ///
    /// `resolve` refuses a non-strict pair before it ever builds these, so
    /// the disjointness check would be unreachable — and therefore
    /// unprovable — if it were only tested that way. It is kept as a type
    /// invariant anyway: it is what makes round 1's defect (one reading
    /// that is at once hot enough to pause and cool enough to resume)
    /// unrepresentable even if `resolve`'s pair precondition were ever
    /// relaxed, which is precisely the kind of change the last three
    /// rounds each made.
    #[test]
    fn pause_bands_refuses_an_entry_that_overlaps_its_recovery() {
        let whole = |lo, hi| Band { lo, hi, universe_hi: SOFT_MAX };
        // fair..=serious entry with nominal..=fair recovery: `fair` is in
        // both. This is `pause_at = fair, resume_at = fair`.
        assert_eq!(PauseBands::new(whole(1, 2), whole(0, 1)), None);
        // …and the same overlap by one reading at the other end.
        assert_eq!(PauseBands::new(whole(0, 1), whole(0, 0)), None);
        // Disjoint: serious entry, nominal..=fair recovery — the default.
        assert!(PauseBands::new(whole(2, 2), whole(0, 1)).is_some());
    }

    /// **The class-regression net.** Every `(pause_at, resume_at)` pair over
    /// the known states PLUS unrecognized names in either slot; for every
    /// tier, the band is either disarmed or has at least one reading in it
    /// AND at least one reading outside it.
    ///
    /// This is the assertion the three shipped defects each violated, and
    /// the reason a fourth instance would be a test failure rather than a
    /// review finding.
    #[test]
    fn every_threshold_pair_yields_bands_that_are_neither_empty_nor_total() {
        let tokens: Vec<&str> =
            THERMAL_STATES.iter().copied().chain(["seroius", "", "Serious", "unknown-9"]).collect();
        let mut armed_pairs = 0;
        for pause_at in &tokens {
            for resume_at in &tokens {
                let bands = ThermalBands::resolve(pause_at, resume_at);
                let label = format!("pause_at={pause_at:?} resume_at={resume_at:?}");

                if !bands.any_armed() {
                    assert!(
                        !bands.disarm_notes().is_empty(),
                        "{label}: a disarmed ladder must say why"
                    );
                }
                if bands.duty().is_none() || bands.pause().is_none() {
                    assert!(
                        !bands.disarm_notes().is_empty(),
                        "{label}: a disarmed tier must say why"
                    );
                }

                let mut check = |b: Band, which: &str| {
                    armed_pairs += 1;
                    let inside = b.a_member();
                    let outside = b.a_non_member();
                    assert!(b.contains(inside), "{label}: {which} claims {inside:?} but excludes it");
                    assert!(
                        !b.contains(outside),
                        "{label}: {which} has no reachable complement — {outside:?} should be \
                         outside it"
                    );
                    // The witnesses must be real readings, not ranks off
                    // the end of the universe.
                    assert!(SoftReading::all().any(|r| r == inside), "{label}: {which} member");
                    assert!(SoftReading::all().any(|r| r == outside), "{label}: {which} non-member");
                    // And the witnesses must agree with an exhaustive sweep
                    // — a `contains` that disagreed with them would make
                    // the guarantee decorative.
                    assert!(
                        SoftReading::all().any(|r| b.contains(r)),
                        "{label}: {which} is uninhabited"
                    );
                    assert!(
                        SoftReading::all().any(|r| !b.contains(r)),
                        "{label}: {which} is total"
                    );
                };

                if let Some(d) = bands.duty() {
                    check(d, "the duty band");
                }
                if let Some(p) = bands.pause() {
                    check(p.entry(), "the pause-entry band");
                    check(p.recovery(), "the recovery band");
                    assert!(
                        !SoftReading::all().any(|r| p.entry().contains(r) && p.recovery().contains(r)),
                        "{label}: a reading is both hot enough to pause and cool enough to resume"
                    );
                }
            }
        }
        assert!(armed_pairs > 0, "the sweep must actually exercise some armed bands");
    }

    /// The four config shapes the four review rounds each shipped, named
    /// individually so a regression says WHICH one came back.
    #[test]
    fn the_four_shipped_defect_configs_each_disarm_the_tier_they_broke() {
        // Round 1: `fair`/`fair` — entry and recovery overlapped, so one
        // unchanging reading manufactured an episode per hold.
        let r1 = ThermalBands::resolve("fair", "fair");
        assert!(!r1.any_armed(), "an equal pair arms nothing");

        // Round 2: `pause_at = nominal` — the entry band covered every
        // reading, so recovery was unreachable.
        let r2 = ThermalBands::resolve("nominal", "nominal");
        assert!(!r2.any_armed());
        assert_eq!(Band::new(0, SOFT_MAX, SOFT_MAX), None, "the entry band itself is refused");

        // Round 3: a typo'd `pause_at` used to rank ABOVE critical and arm
        // the ladder with tiers 3/4 unreachable.
        let r3 = ThermalBands::resolve("seroius", "fair");
        assert!(!r3.any_armed(), "an unrecognized token arms nothing");
        assert!(r3.disarm_notes()[0].why.contains("unrecognized"));

        // Round 4: `resume_at = nominal` — the duty band covered every
        // reading below `pause_at`, so the duty cycle could not be exited.
        let r4 = ThermalBands::resolve("serious", "nominal");
        assert!(r4.duty().is_none(), "tier 2 must be disarmed, not enterable-and-permanent");
        assert!(r4.pause().is_some(), "…while tiers 3/4 stay armed: serious pauses, nominal recovers");
        assert_eq!(r4.disarm_notes().len(), 1);
        assert_eq!(r4.disarm_notes()[0].tiers, "tier 2");
    }

    /// The shipped default pair arms every tier, with the bands the module
    /// doc's tier table describes.
    #[test]
    fn the_default_pair_arms_all_three_tiers() {
        let b = ThermalBands::resolve("serious", "fair");
        let duty = b.duty().expect("tier 2 armed");
        let pause = b.pause().expect("tiers 3/4 armed");
        assert!(b.disarm_notes().is_empty());
        let n = SoftReading::of("nominal").unwrap();
        let f = SoftReading::of("fair").unwrap();
        let s = SoftReading::of("serious").unwrap();
        // tier 1: nominal, no delay.
        assert!(!duty.contains(n) && !pause.entry().contains(n));
        assert!(pause.recovery().contains(n));
        // tier 2: fair duty-cycles.
        assert!(duty.contains(f));
        assert!(!pause.entry().contains(f));
        assert!(pause.recovery().contains(f), "fair is also where a pause recovers to");
        // tiers 3/4: serious pauses, and is NOT a recovery.
        assert!(pause.entry().contains(s));
        assert!(!pause.recovery().contains(s));
        assert!(!duty.contains(s), "serious is tier 3's, not tier 2's");
    }

    /// `pause_at = critical` disarms tiers 3/4 (the breaker owns that
    /// reading) but leaves tier 2 running over what is left — the band's
    /// ceiling falls back to the soft universe's own when nothing returns
    /// ahead of it.
    #[test]
    fn pause_at_critical_disarms_only_the_pause_tiers() {
        let b = ThermalBands::resolve("critical", "fair");
        assert!(b.pause().is_none());
        let duty = b.duty().expect("tier 2 still has a band");
        assert!(duty.contains(SoftReading::of("fair").unwrap()));
        assert!(duty.contains(SoftReading::of("serious").unwrap()));
        assert!(!duty.contains(SoftReading::of("nominal").unwrap()));
        assert_eq!(b.disarm_notes().len(), 1);
        assert_eq!(b.disarm_notes()[0].tiers, "tiers 3 and 4");
    }
}
