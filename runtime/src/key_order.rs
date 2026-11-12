//! (#3112) One JSON key order, whichever build this is.
//!
//! The runtime ships two ways: the standalone `darkmux-runtime` (the Docker
//! image), and linked into the darkmux binary (`darkmux runtime`, what a
//! Seatbelt sandbox runs). In the darkmux build cargo unifies `serde_json`'s
//! features across the whole graph, and `agent-client-protocol` asks for
//! `preserve_order`: a `serde_json::Map` then keeps insertion order where the
//! standalone build keeps every object sorted. Where a `Value` leaves the
//! process (a request body, a model-facing schema, a record), that would
//! change the bytes a model is sent.
//!
//! [`sorted`] puts every object in the order the standalone build already
//! has. `Value::sort_all_objects` is compiled out of serde_json without
//! `preserve_order`, so in the standalone build this does no work at all:
//! its output is unchanged by construction, and the darkmux build's matches
//! it. `tests/runtime_parity.rs` at the repo root runs both builds on the
//! same fixtures and compares the bytes.

use serde_json::Value;

/// `value` with every object, at every depth, in key order.
pub fn sorted(mut value: Value) -> Value {
    value.sort_all_objects();
    value
}

/// An optional `Value` field, [`sorted`] as it is read: for an opaque value a
/// reply hands over and the runtime keeps (a provider's `extra_content`), so
/// every copy of it (the history sent back, the checkpoint) has one order.
pub fn sorted_option<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    <Option<Value> as serde::Deserialize>::deserialize(d).map(|v| v.map(sorted))
}
