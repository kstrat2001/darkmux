//! The one session identity (4.0).
//!
//! A session id ties a family of flow records together: the viewer joins
//! records by it, the dispatch bookends pair on it, presence keys on it, a
//! budget wait is ended on it. Every session belongs to exactly one RUN, and
//! the type says so: a [`SessionId`] cannot be built without a [`RunId`], so
//! two launches of one config can never share a session, a presence key or a
//! budget record.
//!
//! [`SessionId::wire`] is the only way to a string. It stays inside the fleet
//! charset `[A-Za-z0-9._-]`, begins with its run, and is injective: two
//! different identities never share a wire string ([`SessionId::parse`] reads
//! one back exactly). Grammar, `.`-separated:
//!
//! ```text
//! wire     = run [ "." run-kind ] "." kind { "." field }
//! run-kind = "lab" | "solo"                (absent: a mission run)
//! kind     = "run"                         (no field)
//!          | "phase" | "task" | "step"     (the phase, task or step id)
//!          | "adhoc"                       (role, nonce)
//!          | "relay"                       (peer, the sender's wire)
//! ```
//!
//! Every component is escaped: `[A-Za-z0-9-]` is kept, any other byte is
//! written `_XX` (two uppercase hex digits), so a component never contains
//! the `.` separator. Ids with no `_` or `.` read as written:
//! `review-1790000000-ab12cd.task.probe`.
//!
//! Archives written before 4.0 carry the old free-form strings
//! (`task-{t}-{m}`, `mission-run-{m}-{p}`, …). [`SessionId::parse_legacy`]
//! is the ONE place that reads them; no consumer sniffs a prefix itself.

use std::fmt;

/// A malformed identity: an empty run id, or a wire string outside the
/// grammar in this module's doc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdError(pub(crate) String);

impl fmt::Display for IdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for IdError {}

/// What kind of run a session belongs to. It decides whether the run is a
/// mission (its id is stamped as a record's `mission_id`) or not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RunKind {
    /// A mission instance: `mission launch`, a crew-of-one `dispatch`, an
    /// ACP panel run (its correlation id).
    Mission,
    /// A lab run (`lab run`): its records carry no `mission_id`.
    Lab,
    /// A one-off operation that is its own run and has no mission instance:
    /// a radio route or answer, a phase review.
    Standalone,
}

impl RunKind {
    fn tag(self) -> Option<&'static str> {
        match self {
            RunKind::Mission => None,
            RunKind::Lab => Some("lab"),
            RunKind::Standalone => Some("solo"),
        }
    }
}

/// The run a session belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RunId {
    kind: RunKind,
    id: String,
}

impl RunId {
    fn new(kind: RunKind, id: impl Into<String>) -> Result<Self, IdError> {
        let id = id.into();
        if id.is_empty() {
            return Err(IdError("a run id must be non-empty".to_string()));
        }
        Ok(RunId { kind, id })
    }

    /// A mission run: its id is the mission id.
    pub fn mission(id: impl Into<String>) -> Result<Self, IdError> {
        Self::new(RunKind::Mission, id)
    }

    /// A lab run: its id is the lab run id.
    pub fn lab(id: impl Into<String>) -> Result<Self, IdError> {
        Self::new(RunKind::Lab, id)
    }

    /// A standalone run (see [`RunKind::Standalone`]).
    pub fn standalone(id: impl Into<String>) -> Result<Self, IdError> {
        Self::new(RunKind::Standalone, id)
    }

    pub fn kind(&self) -> RunKind {
        self.kind
    }

    pub fn as_str(&self) -> &str {
        &self.id
    }

    /// The mission this run is, when it is one.
    pub fn mission_id(&self) -> Option<&str> {
        match self.kind {
            RunKind::Mission => Some(&self.id),
            RunKind::Lab | RunKind::Standalone => None,
        }
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.id)
    }
}

/// Which session within its run. Every mint site maps to exactly one
/// variant (see `tests::every_mint_site_maps_to_one_variant`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SessionKind {
    /// The run itself: its whole-run bookends and its lifecycle records.
    Run,
    /// A phase's gate-held coder run (worktree, coder, verify).
    Phase { phase: String },
    /// A task's session: every scheduler lifecycle record of its steps, and
    /// the task-scoped seats (`dispatch.single_shot`, `dispatch.map`).
    Task { task: String },
    /// One step's own dispatch (`dispatch.internal` by default).
    Step { step: String },
    /// A dispatch that is not a graph node of its run: a top-level
    /// `dispatch`, a lab dispatch, a crawl unit, a radio seat, a phase
    /// review, a fleet phase fan-out. `nonce` makes it unique in the run.
    Adhoc { role: String, nonce: String },
    /// A fleet receiver running work a peer submitted: the sender's own
    /// session, and the peer it came from. Its run is a standalone run named
    /// after the sender's, so the receiver groups a sender's run together
    /// but never stamps it as a mission: a submitted job is never
    /// attributed to one of the receiver's own missions, whatever id the
    /// sender names.
    Relay { sender: Box<SessionId>, peer: String },
}

/// One session: which session, within which run. The fields are private so
/// the relay invariant (a relay's run is the standalone twin of its
/// sender's) holds by construction.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId {
    kind: SessionKind,
    run: RunId,
}

impl SessionId {
    /// The run's own session.
    pub fn run(run: RunId) -> Self {
        SessionId { kind: SessionKind::Run, run }
    }

    pub fn phase(run: RunId, phase: impl Into<String>) -> Self {
        SessionId { kind: SessionKind::Phase { phase: phase.into() }, run }
    }

    pub fn task(run: RunId, task: impl Into<String>) -> Self {
        SessionId { kind: SessionKind::Task { task: task.into() }, run }
    }

    pub fn step(run: RunId, step: impl Into<String>) -> Self {
        SessionId { kind: SessionKind::Step { step: step.into() }, run }
    }

    pub fn adhoc(run: RunId, role: impl Into<String>, nonce: impl Into<String>) -> Self {
        SessionId { kind: SessionKind::Adhoc { role: role.into(), nonce: nonce.into() }, run }
    }

    /// A receiver's session for work `peer` submitted under `sender`. Its
    /// run is the standalone twin of the sender's (see
    /// [`SessionKind::Relay`]).
    pub fn relay(sender: SessionId, peer: impl Into<String>) -> Self {
        let run = RunId { kind: RunKind::Standalone, id: sender.run.id.clone() };
        SessionId { kind: SessionKind::Relay { sender: Box::new(sender), peer: peer.into() }, run }
    }

    pub fn kind(&self) -> &SessionKind {
        &self.kind
    }

    pub fn run_id(&self) -> &RunId {
        &self.run
    }

    /// The mission this session's run is, when it is one: what a record
    /// under this session carries as `mission_id`.
    pub fn mission_id(&self) -> Option<&str> {
        self.run.mission_id()
    }

    /// The graph step this session is, when it is a step session.
    pub fn step_id(&self) -> Option<&str> {
        match &self.kind {
            SessionKind::Step { step } => Some(step),
            SessionKind::Run
            | SessionKind::Phase { .. }
            | SessionKind::Task { .. }
            | SessionKind::Adhoc { .. }
            | SessionKind::Relay { .. } => None,
        }
    }

    /// The wire string: the only way from a session to a string. See the
    /// module doc for the grammar.
    pub fn wire(&self) -> String {
        let mut parts = vec![escape(&self.run.id)];
        if let Some(tag) = self.run.kind.tag() {
            parts.push(tag.to_string());
        }
        match &self.kind {
            SessionKind::Run => parts.push("run".to_string()),
            SessionKind::Phase { phase } => parts.extend(["phase".to_string(), escape(phase)]),
            SessionKind::Task { task } => parts.extend(["task".to_string(), escape(task)]),
            SessionKind::Step { step } => parts.extend(["step".to_string(), escape(step)]),
            SessionKind::Adhoc { role, nonce } => parts.extend(["adhoc".to_string(), escape(role), escape(nonce)]),
            SessionKind::Relay { sender, peer } => {
                parts.extend(["relay".to_string(), escape(peer), escape(&sender.wire())])
            }
        }
        parts.join(".")
    }

    /// Read a wire string back: the exact inverse of [`SessionId::wire`].
    /// Strict: anything [`SessionId::wire`] would not have written is an
    /// error.
    pub fn parse(wire: &str) -> Result<Self, IdError> {
        let bad = |why: &str| IdError(format!("session id {wire:?}: {why}"));
        let comps: Vec<&str> = wire.split('.').collect();
        let (run, rest) = Self::parse_run(&comps, &bad)?;
        let (tag, fields) = rest.split_first().ok_or_else(|| bad("it names no session kind"))?;
        Self::parse_kind(run, tag, fields, &bad)
    }

    /// The run a wire string begins with, and the components after it.
    fn parse_run<'a>(
        comps: &'a [&'a str],
        bad: &dyn Fn(&str) -> IdError,
    ) -> Result<(RunId, &'a [&'a str]), IdError> {
        let run_id = unescape(comps[0]).ok_or_else(|| bad("its run is not escaped as written"))?;
        let (run_kind, rest) = match comps.get(1).copied() {
            Some("lab") => (RunKind::Lab, &comps[2..]),
            Some("solo") => (RunKind::Standalone, &comps[2..]),
            _ => (RunKind::Mission, &comps[1..]),
        };
        let run = RunId::new(run_kind, run_id).map_err(|e| bad(&e.0))?;
        Ok((run, rest))
    }

    /// The session within `run` that a kind `tag` and its escaped `fields`
    /// name.
    fn parse_kind(run: RunId, tag: &str, fields: &[&str], bad: &dyn Fn(&str) -> IdError) -> Result<Self, IdError> {
        let tag = WireKind::parse(tag).ok_or_else(|| bad("unknown session kind"))?;
        if fields.len() != tag.arity() {
            return Err(bad("wrong number of fields"));
        }
        let f: Vec<String> = fields
            .iter()
            .map(|f| unescape(f))
            .collect::<Option<_>>()
            .ok_or_else(|| bad("a field is not escaped as written"))?;
        let id = match tag {
            WireKind::Run => SessionId::run(run),
            WireKind::Phase => SessionId::phase(run, &f[0]),
            WireKind::Task => SessionId::task(run, &f[0]),
            WireKind::Step => SessionId::step(run, &f[0]),
            WireKind::Adhoc => SessionId::adhoc(run, &f[0], &f[1]),
            WireKind::Relay => {
                let relay = SessionId::relay(SessionId::parse(&f[1])?, &f[0]);
                if relay.run != run {
                    return Err(bad("a relay's run is not its sender's, standalone"));
                }
                relay
            }
        };
        Ok(id)
    }

    /// Read a session id from a record of ANY age, for attribution: a
    /// current wire string exactly as [`SessionId::parse`] does, else one of
    /// the pre-4.0 free-form strings, placed by the record's own
    /// `mission_id` (the only way to split an old string that embedded it):
    ///
    /// - `{m}` and `mission-{m}`: the run's own session.
    /// - `mission-run-{m}-{p}`: phase `p`'s coder run.
    /// - `task-{t}` and `task-{t}-{m}`: task `t`.
    /// - `step-{s}` and `step-{s}-{m}`: step `s`.
    /// - anything else under a mission: an ad-hoc dispatch of that run,
    ///   whose nonce is the old string.
    ///
    /// `None` for an old string with no `mission_id` to place it in: a
    /// pre-4.0 record of no run cannot be attributed to one.
    pub fn parse_legacy(wire: &str, mission_id: Option<&str>) -> Option<Self> {
        if let Ok(id) = SessionId::parse(wire) {
            return Some(id);
        }
        let mid = mission_id?;
        let run = RunId::mission(mid).ok()?;
        let unscoped = |rest: &str| rest.strip_suffix(&format!("-{mid}")).unwrap_or(rest).to_string();
        if wire == mid || wire.strip_prefix("mission-") == Some(mid) {
            return Some(SessionId::run(run));
        }
        if let Some(phase) = wire.strip_prefix(&format!("mission-run-{mid}-")) {
            return Some(SessionId::phase(run, phase));
        }
        if let Some(task) = wire.strip_prefix("task-") {
            return Some(SessionId::task(run, unscoped(task)));
        }
        if let Some(step) = wire.strip_prefix("step-") {
            return Some(SessionId::step(run, unscoped(step)));
        }
        Some(SessionId::adhoc(run, "", wire))
    }
}

/// A session kind's tag in the wire grammar (see the module doc), read once
/// by [`SessionId::parse`].
#[derive(Debug, Clone, Copy)]
enum WireKind {
    Run,
    Phase,
    Task,
    Step,
    Adhoc,
    Relay,
}

impl WireKind {
    fn parse(tag: &str) -> Option<Self> {
        match tag {
            "run" => Some(WireKind::Run),
            "phase" => Some(WireKind::Phase),
            "task" => Some(WireKind::Task),
            "step" => Some(WireKind::Step),
            "adhoc" => Some(WireKind::Adhoc),
            "relay" => Some(WireKind::Relay),
            _ => None,
        }
    }

    /// How many escaped fields follow the tag.
    fn arity(self) -> usize {
        match self {
            WireKind::Run => 0,
            WireKind::Phase | WireKind::Task | WireKind::Step => 1,
            WireKind::Adhoc | WireKind::Relay => 2,
        }
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.wire())
    }
}

/// Serialized as its wire string; read back strictly.
impl serde::Serialize for SessionId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.wire())
    }
}

impl<'de> serde::Deserialize<'de> for SessionId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let wire = String::deserialize(d)?;
        SessionId::parse(&wire).map_err(serde::de::Error::custom)
    }
}

/// Which session a step kind's own dispatch records land under, as the kind
/// DECLARES it. The run comes from whoever runs the step, never from the
/// kind: [`SessionScope::session`] composes the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionScope {
    /// The kind never dispatches (`procedural.*`, store operations).
    None,
    /// The step's own session ([`SessionKind::Step`]).
    Step,
    /// The owning task's session ([`SessionKind::Task`]): sibling seats of
    /// one task share it.
    Task,
}

impl SessionScope {
    /// The session a step with these ids dispatches under, in `run`.
    pub fn session(self, run: &RunId, task_id: &str, step_id: &str) -> Option<SessionId> {
        match self {
            SessionScope::None => None,
            SessionScope::Step => Some(SessionId::step(run.clone(), step_id)),
            SessionScope::Task => Some(SessionId::task(run.clone(), task_id)),
        }
    }
}

/// Keep `[A-Za-z0-9-]`; write any other byte as `_XX`.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' {
            out.push(b as char);
        } else {
            out.push_str(&format!("_{b:02X}"));
        }
    }
    out
}

/// The inverse of [`escape`], refusing anything `escape` would not write
/// (a kept byte escaped, lowercase hex, a stray `_`, a non-UTF-8 result).
fn unescape(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_alphanumeric() || b == b'-' {
            out.push(b);
            i += 1;
            continue;
        }
        if b != b'_' {
            return None;
        }
        let hex = s.get(i + 1..i + 3)?;
        if hex.bytes().any(|h| h.is_ascii_lowercase()) {
            return None;
        }
        let v = u8::from_str_radix(hex, 16).ok()?;
        if v.is_ascii_alphanumeric() || v == b'-' {
            return None;
        }
        out.push(v);
        i += 3;
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn m(id: &str) -> RunId {
        RunId::mission(id).unwrap()
    }

    fn in_fleet_charset(s: &str) -> bool {
        !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    }

    /// Every variant, one example each, with the production mint each one
    /// stands for. The exhaustive match fails to compile when a variant is
    /// added without a row here.
    #[test]
    fn every_mint_site_maps_to_one_variant() {
        let run = m("review-1790000000-ab12cd");
        let rows = [
            (SessionId::run(run.clone()), "the run's bookends and lifecycle (was `mission-{m}` and bare `{m}`)"),
            (SessionId::phase(run.clone(), "p1"), "coder-phase gate run (was `mission-run-{m}-{p}`)"),
            (SessionId::task(run.clone(), "t1"), "task lifecycle and task-scoped seats (was `task-{t}[-{m}]`)"),
            (SessionId::step(run.clone(), "s1"), "a step's own dispatch (was `step-{s}[-{m}]`)"),
            (SessionId::adhoc(run.clone(), "coder", "1790000000000000-0"), "top-level dispatch, crawl, fan-out"),
            (SessionId::relay(SessionId::step(run.clone(), "s1"), "studio"), "fleet receiver (was `{s}-from-{peer}`)"),
        ];
        let is_relay = |id: &SessionId| matches!(id.kind(), SessionKind::Relay { .. });
        for (id, what) in &rows {
            let tag = match id.kind() {
                SessionKind::Run => "run",
                SessionKind::Phase { .. } => "phase",
                SessionKind::Task { .. } => "task",
                SessionKind::Step { .. } => "step",
                SessionKind::Adhoc { .. } => "adhoc",
                SessionKind::Relay { .. } => "relay",
            };
            let wire = id.wire();
            assert!(wire.starts_with("review-1790000000-ab12cd."), "{what}: begins with its run: {wire}");
            assert!(wire.split('.').any(|c| c == tag), "{what}: names its kind: {wire}");
            assert_eq!(SessionId::parse(&wire).as_ref(), Ok(id), "{what}: reads back");
            let want = if is_relay(id) { None } else { Some("review-1790000000-ab12cd") };
            assert_eq!(id.mission_id(), want, "{what}: a relay is never one of the receiver's missions");
        }
        assert_eq!(rows[3].0.step_id(), Some("s1"));
        assert_eq!(rows.iter().filter(|(id, _)| id.step_id().is_some()).count(), 1, "only a step session names a step");
    }

    #[test]
    fn a_run_names_its_kind_and_only_a_mission_run_is_a_mission() {
        let lab = SessionId::adhoc(RunId::lab("quick-q-p-1790000000-0").unwrap(), "coder", "n");
        let solo = SessionId::adhoc(RunId::standalone("radio-1").unwrap(), "radio-router", "n");
        assert_eq!(lab.wire(), "quick-q-p-1790000000-0.lab.adhoc.coder.n");
        assert_eq!(solo.wire(), "radio-1.solo.adhoc.radio-router.n");
        assert_eq!((lab.mission_id(), solo.mission_id()), (None, None));
        assert_eq!(SessionId::parse(&lab.wire()), Ok(lab));
        assert!(RunId::mission("").is_err(), "no run without an id");
    }

    #[test]
    fn two_runs_of_one_config_never_share_a_session() {
        for scope in [SessionScope::Step, SessionScope::Task] {
            let a = scope.session(&m("launch-a"), "t1", "s1").unwrap();
            let b = scope.session(&m("launch-b"), "t1", "s1").unwrap();
            assert_ne!(a.wire(), b.wire(), "{scope:?}");
        }
        assert_eq!(SessionScope::None.session(&m("launch-a"), "t1", "s1"), None);
    }

    /// A deterministic generator (a fixed-seed LCG: no clock, no
    /// dependency) over an adversarial alphabet: the separator, the escape
    /// byte, look-alike hex, the tag words, multi-byte text, empty strings.
    struct Gen(u64);
    impl Gen {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            self.0 >> 33
        }
        fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
            xs[(self.next() as usize) % xs.len()]
        }
        fn comp(&mut self) -> String {
            const ATOMS: &[&str] = &[
                "", ".", "..", "_", "_2E", "_2e", "-", "a", "Z9", "run", "task", "lab", "solo", "relay", "é", "/", " ",
                "mission-", "step-",
            ];
            (0..self.next() % 4).map(|_| self.pick(ATOMS)).collect()
        }
        fn run(&mut self) -> RunId {
            let id = format!("r{}", self.comp());
            match self.next() % 3 {
                0 => RunId::mission(id),
                1 => RunId::lab(id),
                _ => RunId::standalone(id),
            }
            .unwrap()
        }
        fn session(&mut self, depth: u32) -> SessionId {
            let run = self.run();
            match self.next() % if depth == 0 { 5 } else { 6 } {
                0 => SessionId::run(run),
                1 => SessionId::phase(run, self.comp()),
                2 => SessionId::task(run, self.comp()),
                3 => SessionId::step(run, self.comp()),
                4 => SessionId::adhoc(run, self.comp(), self.comp()),
                _ => SessionId::relay(self.session(depth - 1), self.comp()),
            }
        }
    }

    /// `wire()` stays in the fleet charset and is injective: across 20 000
    /// generated identities of every variant, no two different identities
    /// share a wire string, and every wire string reads back to its own.
    #[test]
    fn wire_stays_in_the_fleet_charset_and_is_injective() {
        let mut g = Gen(0x5eed);
        let mut seen: HashMap<String, SessionId> = HashMap::new();
        for _ in 0..20_000 {
            let id = g.session(2);
            let wire = id.wire();
            assert!(in_fleet_charset(&wire), "outside the fleet charset: {wire:?}");
            assert!(!wire.starts_with('.'), "a leading dot is never a file-name segment: {wire:?}");
            assert_eq!(SessionId::parse(&wire).as_ref(), Ok(&id), "reads back: {wire:?}");
            if let Some(prev) = seen.insert(wire.clone(), id.clone()) {
                assert_eq!(prev, id, "two identities share {wire:?}");
            }
        }
        assert!(seen.len() > 5_000, "the generator must actually vary: {}", seen.len());
    }

    #[test]
    fn parse_refuses_what_wire_never_writes() {
        for bad in [
            "",
            "m",
            "m.bogus",
            "m.run.extra",
            "m.task",
            "m.task.a.b",
            "m.task.a_2e",
            "m.task.a_41",
            "m.task.a_",
            "m.task.a b",
            ".task.t",
            "m.lab",
            "m.relay.peer.m_2Erun",
            "m.solo.relay.peer.other_2Erun",
        ] {
            assert!(SessionId::parse(bad).is_err(), "{bad:?} must not parse");
        }
    }

    /// Pre-4.0 archive strings still attribute, given the record's own
    /// `mission_id`.
    #[test]
    fn legacy_archive_strings_still_attribute() {
        let mid = Some("m-1");
        let run = m("m-1");
        let cases = [
            ("m-1", SessionId::run(run.clone())),
            ("mission-m-1", SessionId::run(run.clone())),
            ("mission-run-m-1-m-1-p1", SessionId::phase(run.clone(), "m-1-p1")),
            ("task-build", SessionId::task(run.clone(), "build")),
            ("task-build-m-1", SessionId::task(run.clone(), "build")),
            ("task-m-1-task", SessionId::task(run.clone(), "m-1-task")),
            ("step-s1", SessionId::step(run.clone(), "s1")),
            ("step-s1-m-1", SessionId::step(run.clone(), "s1")),
            (
                "crew-dispatch-coder-1790000000000000-0",
                SessionId::adhoc(run.clone(), "", "crew-dispatch-coder-1790000000000000-0"),
            ),
        ];
        for (old, want) in cases {
            assert_eq!(SessionId::parse_legacy(old, mid), Some(want), "{old}");
        }
        assert_eq!(SessionId::parse_legacy("step-s1", None), None, "no run to place it in");
        let current = SessionId::step(m("m-2"), "s1");
        assert_eq!(SessionId::parse_legacy(&current.wire(), mid), Some(current), "a current string reads as written");
    }

    #[test]
    fn serde_carries_the_wire_string() {
        let id = SessionId::task(m("m-1"), "t_1");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"m-1.task.t_5F1\"");
        assert_eq!(serde_json::from_str::<SessionId>(&json).unwrap(), id);
        assert!(serde_json::from_str::<SessionId>("\"task-t1\"").is_err(), "a legacy string is not a current id");
    }
}
