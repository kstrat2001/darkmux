//! (#2301) `Output<T>` — the envelope every TYPED step output rides in.
//!
//! **The rule, stated once.** Every value one step kind hands to another is
//! a TYPED serde struct with a `schema_version`; required fields are plain,
//! optional fields carry `#[serde(default)]`. The consumer deserializes
//! through that struct, and **that deserialize IS the validation**: a
//! producer that drifted fails at the read, by field name, instead of being
//! silently summarized as zeros. There is no free-form JSON blob and no
//! string protocol between kinds.
//!
//! This module adds the thin part around the body: WHO produced it, WHEN,
//! and WHAT it is. `kind` is a content id (`"crawl.plan"`,
//! `"crawl.unit-outcome"`, `"crawl.summary"`) the reader checks against the
//! value it expects BEFORE deserializing `body` — a mismatch is a refusal
//! naming both, which is what turns a mis-wired graph into an error message
//! rather than a confusing parse failure deep inside a body struct.
//!
//! **Ports stay labels.** A `Port`'s name says where a value flows, never
//! what is in it; `kind` here is what says what is in it. The two are
//! deliberately separate.
//!
//! **Reading.** A step's `output` is a STRING, and today the crawl's plan
//! step outputs a PATH to a file. [`Output::read`] accepts all three shapes
//! so the transition needs no flag day:
//!
//! - inline JSON — an `Output<T>` envelope, or a bare `T` (a pre-wrapper
//!   producer);
//! - `{"ref": {"path": "…"}}` — a pointer to a file holding either of the
//!   above;
//! - a bare path string — the crawl plan's shape today.
//!
//! **Integrity.** Every envelope carries `hash` — blake3 over its own
//! body, canonicalized through `serde_json::Value` so field order can never
//! change the digest — and [`Output::read`] recomputes it and refuses a
//! mismatch. A consumer has to be able to tell a COMPLETE file from a
//! partial one, and a STALE copy from the current one, whatever moved it
//! there; a length check cannot, and a timestamp lies. Bodies are written
//! once (tmp + rename) and never rewritten, so a body whose hash disagrees
//! is a truncated write or a copy that is not the one this run produced —
//! either way, not something to read. A synced or shared filesystem
//! (iCloud, a network share) is NEVER the transport for a `ref`: those
//! deliver partial files as ordinary reads, which is exactly the case this
//! check is here to name.
//!
//! **Fleet transport comes later.** Once every producer wraps, a `ref` can
//! name a machine as well as a path and be fetched from the producing
//! machine's daemon. Nothing here does that yet; the shape is what makes it
//! possible without changing any body struct.

use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// [`Output`]'s own schema version — the ENVELOPE's, never the body's. A
/// body carries its own `schema_version` and versions independently.
pub const OUTPUT_SCHEMA_VERSION: &str = "1.0";

/// Who produced one step output. Every field is best-effort at the
/// producer: a value that genuinely is not known is empty rather than
/// invented, because provenance that guesses is worse than provenance that
/// admits a gap.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct Producer {
    /// The run this output belongs to.
    #[serde(default)]
    pub mission: String,
    #[serde(default)]
    pub task: String,
    #[serde(default)]
    pub step: String,
    /// The machine that produced it — the SAME `machine_id` every flow
    /// record is stamped with (`darkmux_types::config_access`), so an
    /// output and the records around it join without a second identity.
    #[serde(default)]
    pub machine_id: String,
}

impl Producer {
    /// The producer triple for a step, with `machine_id` read from the one
    /// place every other darkmux surface reads it.
    pub fn of(mission: &str, task: &str, step: &str) -> Self {
        Self {
            mission: mission.to_string(),
            task: task.to_string(),
            step: step.to_string(),
            machine_id: darkmux_types::config_access::machine_id().unwrap_or_default(),
        }
    }
}

/// One typed step output: a body plus who made it and what it is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
#[serde(bound(serialize = "T: Serialize", deserialize = "T: DeserializeOwned"))]
pub struct Output<T> {
    /// The ENVELOPE's version ([`OUTPUT_SCHEMA_VERSION`]).
    pub schema_version: String,
    /// The content id — what `body` is. Checked against the reader's
    /// expectation before `body` is looked at.
    pub kind: String,
    pub producer: Producer,
    /// RFC3339, stamped at wrap time.
    pub produced_at: String,
    /// blake3 (hex) over the canonicalized body — see the module doc's
    /// "Integrity". Empty only on a body read without an envelope.
    #[serde(default)]
    pub hash: String,
    pub body: T,
}

/// The digest a body is identified by: blake3 over the body canonicalized
/// through `serde_json::Value`, whose object keys are sorted, so a
/// producer's struct field order and a reader's parse order agree.
pub fn body_hash(body: &serde_json::Value) -> String {
    blake3::hash(serde_json::to_string(body).unwrap_or_default().as_bytes()).to_hex().to_string()
}

impl<T: Serialize> Output<T> {
    /// Wrap `body` as `kind`, stamped and hashed now.
    pub fn wrap(kind: &str, body: T, producer: Producer) -> Self {
        let hash = serde_json::to_value(&body).map(|v| body_hash(&v)).unwrap_or_default();
        Self {
            schema_version: OUTPUT_SCHEMA_VERSION.to_string(),
            kind: kind.to_string(),
            producer,
            produced_at: darkmux_flow::ts_utc_now(),
            hash,
            body,
        }
    }

    /// The envelope as the string a `StepOutcome::output` carries.
    pub fn to_output_string(&self) -> Result<String> {
        serde_json::to_string(self).context("serializing a step output envelope")
    }
}

impl<T: DeserializeOwned> Output<T> {
    /// Read a step's `output` string as an `Output<T>` of `expected_kind`.
    ///
    /// Accepts the three shapes this module's doc names (a wrapped
    /// envelope, a `{"ref": {"path": …}}` pointer, or a bare path to a
    /// file holding either) plus one transition shape: a bare `T` with no
    /// envelope at all, which is what a pre-wrapper producer wrote. A bare
    /// body cannot be kind-checked — there is nothing to check — so it is
    /// accepted with the expected kind assumed, and every other shape IS
    /// checked.
    pub fn read(raw: &str, expected_kind: &str) -> Result<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            bail!("step output: expected a `{expected_kind}` output, got an empty string");
        }
        if raw.starts_with('{') {
            let doc: serde_json::Value = serde_json::from_str(raw)
                .with_context(|| format!("step output: parsing a `{expected_kind}` output as JSON"))?;
            if let Some(path) = ref_path(&doc) {
                return Self::read_path(Path::new(&path), expected_kind);
            }
            return Self::from_value(doc, expected_kind, "the step's own output");
        }
        Self::read_path(Path::new(raw), expected_kind)
    }

    /// Read `path`'s contents as an `Output<T>` of `expected_kind`.
    pub fn read_path(path: &Path, expected_kind: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| {
            format!(
                "step output: reading `{expected_kind}` from {} — a producing step's output must \
                 be inline JSON or a readable path to it",
                path.display()
            )
        })?;
        let doc: serde_json::Value = serde_json::from_str(&text)
            .with_context(|| format!("step output: {} is not valid JSON", path.display()))?;
        Self::from_value(doc, expected_kind, &path.display().to_string())
    }

    /// The kind check, then the body read — in that order, so a mis-wired
    /// graph is named as one rather than surfacing as a body field error.
    fn from_value(doc: serde_json::Value, expected_kind: &str, whence: &str) -> Result<Self> {
        let looks_wrapped = doc.get("kind").is_some() && doc.get("body").is_some();
        if !looks_wrapped {
            // Transition: a bare body from a producer that predates the
            // wrapper. Nothing to kind-check; the body read still is.
            let body: T = serde_json::from_value(doc).with_context(|| {
                format!("step output: reading {whence} as an unwrapped `{expected_kind}` body")
            })?;
            return Ok(Self {
                schema_version: OUTPUT_SCHEMA_VERSION.to_string(),
                kind: expected_kind.to_string(),
                producer: Producer::default(),
                produced_at: String::new(),
                // Nothing was stamped, so nothing is claimed — an empty
                // hash reads as "unverified", never as "verified empty".
                hash: String::new(),
                body,
            });
        }
        let found = doc.get("kind").and_then(serde_json::Value::as_str).unwrap_or("");
        if found != expected_kind {
            bail!(
                "step output: {whence} is a `{found}` output, but a `{expected_kind}` was \
                 expected — the graph wires this step to the wrong producer"
            );
        }
        // Integrity BEFORE the body read: a truncated or stale file must be
        // named as one, not surfaced as a confusing field error.
        let claimed = doc.get("hash").and_then(serde_json::Value::as_str).unwrap_or("");
        if !claimed.is_empty() {
            let actual = doc.get("body").map(body_hash).unwrap_or_default();
            if actual != claimed {
                bail!(
                    "step output: {whence} is a `{expected_kind}` whose body does not match its \
                     own hash — expected {}…, got {}…. A body is written once and never \
                     rewritten, so this is a partial write or a copy that is not the one this run \
                     produced; it is not safe to read",
                    &claimed[..claimed.len().min(12)],
                    &actual[..actual.len().min(12)]
                );
            }
        }
        serde_json::from_value(doc)
            .with_context(|| format!("step output: reading {whence} as a `{expected_kind}` envelope"))
    }
}

/// Resolve a step's `output` string to the JSON document it names —
/// following a `{"ref": {"path": …}}` pointer or a bare path, or parsing
/// inline JSON. Returns the document and a human name for it (the path
/// when there was one), for error messages.
///
/// This is the untyped half, for the one consumer that legitimately does
/// not know the body type: the graph's `grow` seam, which only needs an
/// items ARRAY out of whatever a producer wrote.
pub fn resolve_output_doc(raw: &str) -> Result<(serde_json::Value, String)> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("step output: a producing step's output is empty — nothing to read");
    }
    let read_file = |p: &str| -> Result<(serde_json::Value, String)> {
        let text = std::fs::read_to_string(p).with_context(|| {
            format!(
                "step output: reading {p} — a producing step's output must be inline JSON, a \
                 readable path to it, or a `ref` naming one"
            )
        })?;
        let doc = serde_json::from_str(&text)
            .with_context(|| format!("step output: {p} is not valid JSON"))?;
        Ok((doc, p.to_string()))
    };
    if raw.starts_with('{') {
        let doc: serde_json::Value =
            serde_json::from_str(raw).context("step output: parsing a producing step's output as JSON")?;
        return match ref_path(&doc) {
            Some(p) => read_file(&p),
            None => Ok((doc, "the step's own output".to_string())),
        };
    }
    read_file(raw)
}

/// `{"ref": {"path": "…"}}` -> the path. Any other shape is not a ref.
fn ref_path(doc: &serde_json::Value) -> Option<String> {
    doc.get("ref")?.get("path")?.as_str().map(str::to_string)
}

/// A `{"ref": {"path": …}}` pointer to `path` — the output string a
/// producer writes when its body lives in a file. (The crawl's plan step
/// still writes the bare path; both read.)
pub fn ref_output_string(path: &Path) -> String {
    serde_json::json!({ "ref": { "path": path.display().to_string() } }).to_string()
}

/// The one place a reader turns a "no output at all" into an error that
/// names what it wanted.
pub fn missing_output(what: &str, expected_kind: &str) -> anyhow::Error {
    anyhow!("step output: {what} produced no output, so no `{expected_kind}` could be read")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Body {
        schema_version: String,
        n: u64,
        #[serde(default)]
        note: String,
    }

    fn body() -> Body {
        Body { schema_version: "1.0".into(), n: 7, note: "hi".into() }
    }

    #[test]
    fn an_envelope_round_trips_through_its_own_output_string() {
        let out = Output::wrap("crawl.unit-outcome", body(), Producer::of("m-1", "t-1", "s-1"));
        let text = out.to_output_string().unwrap();
        let back: Output<Body> = Output::read(&text, "crawl.unit-outcome").unwrap();
        assert_eq!(back.body, body());
        assert_eq!(back.kind, "crawl.unit-outcome");
        assert_eq!(back.schema_version, OUTPUT_SCHEMA_VERSION);
        assert_eq!(back.producer.mission, "m-1");
        assert_eq!(back.producer.step, "s-1");
        assert!(!back.produced_at.is_empty(), "stamped at wrap time");
    }

    #[test]
    fn the_wrong_kind_is_refused_naming_both() {
        let text = Output::wrap("crawl.plan", body(), Producer::default()).to_output_string().unwrap();
        let err = Output::<Body>::read(&text, "crawl.summary").unwrap_err().to_string();
        assert!(err.contains("crawl.plan") && err.contains("crawl.summary"), "{err}");
    }

    #[test]
    fn a_body_that_does_not_match_fails_by_field_name() {
        let text = serde_json::json!({
            "schema_version": "1.0", "kind": "crawl.summary", "producer": {},
            "produced_at": "", "body": {"schema_version": "1.0", "note": "no n"}
        })
        .to_string();
        let err = format!("{:#}", Output::<Body>::read(&text, "crawl.summary").unwrap_err());
        assert!(err.contains('n'), "{err}");
    }

    #[test]
    fn a_bare_path_string_reads_the_file_it_names() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("plan.json");
        std::fs::write(&p, serde_json::to_string(&body()).unwrap()).unwrap();
        // An UNWRAPPED body at a path — the crawl plan's shape today.
        let back: Output<Body> = Output::read(&p.display().to_string(), "crawl.plan").unwrap();
        assert_eq!(back.body, body());
        assert_eq!(back.kind, "crawl.plan", "an unwrapped body takes the expected kind");
    }

    #[test]
    fn a_ref_pointer_reads_the_wrapped_envelope_it_names_and_still_kind_checks() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("out.json");
        std::fs::write(&p, Output::wrap("crawl.plan", body(), Producer::default()).to_output_string().unwrap())
            .unwrap();
        let pointer = ref_output_string(&p);
        assert_eq!(Output::<Body>::read(&pointer, "crawl.plan").unwrap().body, body());
        let err = Output::<Body>::read(&pointer, "crawl.unit-outcome").unwrap_err().to_string();
        assert!(err.contains("crawl.plan") && err.contains("crawl.unit-outcome"), "{err}");
    }

    #[test]
    fn an_empty_or_missing_output_names_what_was_wanted() {
        let err = Output::<Body>::read("   ", "crawl.summary").unwrap_err().to_string();
        assert!(err.contains("crawl.summary") && err.contains("empty"), "{err}");
        assert!(missing_output("task `t`", "crawl.plan").to_string().contains("crawl.plan"));
    }

    #[test]
    fn a_byte_flipped_in_a_written_body_is_refused_by_hash() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("plan.json");
        let text = Output::wrap("crawl.plan", body(), Producer::default()).to_output_string().unwrap();
        assert!(Output::<Body>::read_path(&p, "crawl.plan").is_err(), "nothing written yet");
        std::fs::write(&p, &text).unwrap();
        assert_eq!(Output::<Body>::read_path(&p, "crawl.plan").unwrap().body, body());

        // Flip one byte INSIDE the body — still valid JSON, still the right
        // kind, still every required field: only the hash catches it.
        std::fs::write(&p, text.replace("\"n\":7", "\"n\":8")).unwrap();
        let err = Output::<Body>::read_path(&p, "crawl.plan").unwrap_err().to_string();
        assert!(err.contains("does not match its own hash"), "{err}");
        assert!(err.contains("crawl.plan"), "the refusal names the kind: {err}");
    }

    #[test]
    fn field_order_never_changes_the_digest() {
        // The canonicalization is what makes the check usable at all: a
        // producer's struct order and a reader's parse order must agree.
        let a: serde_json::Value = serde_json::from_str(r#"{"a":1,"b":2}"#).unwrap();
        let b: serde_json::Value = serde_json::from_str(r#"{"b":2,"a":1}"#).unwrap();
        assert_eq!(body_hash(&a), body_hash(&b));
        assert_ne!(body_hash(&a), body_hash(&serde_json::json!({"a": 1, "b": 3})));
    }
}
