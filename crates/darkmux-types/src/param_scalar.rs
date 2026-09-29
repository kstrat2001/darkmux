//! The scalar types a mission step's config uses for numbers, flags and ids.
//!
//! A step's config reaches the step kind after `{{param}}` substitution, and
//! a `--param name=value` value is always TEXT: `--param draws=3` puts the
//! string `"3"` where the document wrote `"{{draws}}"`. A number or flag
//! field therefore has to accept both its JSON form (`3`, `true`) and its
//! text form (`"3"`, `"true"`), and the unknown-key gate has to accept a
//! `{{param}}` reference there before substitution. [`Count`] and [`Flag`]
//! are those fields' types (with [`Decimal`] for a float): one parse for the gate
//! ([`count_text_ok`] / [`flag_text_ok`], keyed by the schema `format`) and
//! for the step kind's own load, so they cannot disagree.

use schemars::{json_schema, JsonSchema, Schema, SchemaGenerator};
use serde::de::{self, Deserializer};
use serde::Deserialize;
use std::borrow::Cow;

/// The schema `format` of the text form of a [`Count`].
pub const COUNT_FORMAT: &str = "darkmux-count";
/// The schema `format` of the text form of a [`BlankableCount`]: a [`Count`]'s
/// text form, or nothing.
pub const BLANKABLE_COUNT_FORMAT: &str = "darkmux-count-or-blank";
/// The schema `format` of the text form of a [`Flag`].
pub const FLAG_FORMAT: &str = "darkmux-flag";
/// The schema `format` of the text form of a [`Decimal`].
pub const DECIMAL_FORMAT: &str = "darkmux-decimal";
/// The schema `format` of a [`crate::session_id::SessionId`]'s wire string.
pub const SESSION_ID_FORMAT: &str = "darkmux-session-id";

/// `"{{name}}"` (and nothing else) -> `Some("name")`: the one definition of
/// a whole-string `{{param}}` reference, shared by the mission config's
/// substitution and this module's gate forms.
pub fn whole_placeholder(text: &str) -> Option<&str> {
    let inner = text.strip_prefix("{{")?.strip_suffix("}}")?;
    if inner.contains("{{") || inner.contains("}}") {
        return None;
    }
    Some(inner.trim())
}

/// Whether `text` is an unresolved whole-string `{{param}}` reference, which
/// substitution replaces before a step kind reads it. An embedded one
/// (`"n={{n}}"`) renders into a larger string, which is not a number.
pub fn is_placeholder(text: &str) -> bool {
    whole_placeholder(text).is_some()
}

/// A non-negative integer from its number or its decimal text.
fn parse_count(text: &str) -> Option<u64> {
    text.trim().parse().ok()
}

/// A finite decimal number from its text.
fn parse_decimal(text: &str) -> Option<f64> {
    text.trim().parse::<f64>().ok().filter(|n| n.is_finite())
}

/// A flag from its text: `true`/`false`, `1`/`0`, `yes`/`no`, `on`/`off`, or
/// blank (a `{{param}}` nothing supplied), which reads as off.
fn parse_flag(text: &str) -> Option<bool> {
    match text.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" | "" => Some(false),
        _ => None,
    }
}

/// Whether a string carrying schema `format` is acceptable before the step
/// kind reads it. `None` for a format that is not one of this module's.
pub fn text_form_ok(format: &str, text: &str) -> Option<bool> {
    let ok = match format {
        COUNT_FORMAT => is_placeholder(text) || parse_count(text).is_some(),
        BLANKABLE_COUNT_FORMAT => is_placeholder(text) || text.trim().is_empty() || parse_count(text).is_some(),
        FLAG_FORMAT => is_placeholder(text) || parse_flag(text).is_some(),
        DECIMAL_FORMAT => is_placeholder(text) || parse_decimal(text).is_some(),
        SESSION_ID_FORMAT => crate::session_id::SessionId::parse(text).is_ok(),
        _ => return None,
    };
    Some(ok)
}

/// A non-negative integer, written as a number or as its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Count(pub u64);

impl Count {
    /// The value as a `u32`, `None` past its range.
    pub fn as_u32(self) -> Option<u32> {
        u32::try_from(self.0).ok()
    }

    /// The value as a `u32`, capped at `u32::MAX`.
    pub fn saturating_u32(self) -> u32 {
        u32::try_from(self.0).unwrap_or(u32::MAX)
    }

    /// The value as a `usize`, `None` past its range.
    pub fn as_usize(self) -> Option<usize> {
        usize::try_from(self.0).ok()
    }
}

/// A [`Count`] where a blank text (a `{{param}}` nothing supplied) and
/// `null` both mean "not set".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BlankableCount(pub Option<Count>);

/// A finite decimal number, written as a number or as its text.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Decimal(pub f64);

/// A boolean, written as `true`/`false` or as its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flag(pub bool);

impl<'de> Deserialize<'de> for Count {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match serde_json::Value::deserialize(d)? {
            serde_json::Value::Number(n) => n.as_u64().map(Count).ok_or_else(|| de::Error::custom("must be a non-negative integer")),
            serde_json::Value::String(s) => parse_count(&s)
                .map(Count)
                .ok_or_else(|| de::Error::custom(format!("must be a non-negative integer, got \"{s}\""))),
            other => Err(de::Error::custom(format!("must be a non-negative integer, got {other}"))),
        }
    }
}

impl<'de> Deserialize<'de> for BlankableCount {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match serde_json::Value::deserialize(d)? {
            serde_json::Value::Null => Ok(Self(None)),
            serde_json::Value::String(s) if s.trim().is_empty() => Ok(Self(None)),
            other => Count::deserialize(other).map(|c| Self(Some(c))).map_err(de::Error::custom),
        }
    }
}

impl<'de> Deserialize<'de> for Decimal {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match serde_json::Value::deserialize(d)? {
            serde_json::Value::Number(n) => n.as_f64().map(Decimal).ok_or_else(|| de::Error::custom("must be a number")),
            serde_json::Value::String(s) => parse_decimal(&s)
                .map(Decimal)
                .ok_or_else(|| de::Error::custom(format!("must be a number, got \"{s}\""))),
            other => Err(de::Error::custom(format!("must be a number, got {other}"))),
        }
    }
}

impl<'de> Deserialize<'de> for Flag {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match serde_json::Value::deserialize(d)? {
            serde_json::Value::Bool(b) => Ok(Flag(b)),
            serde_json::Value::String(s) => parse_flag(&s)
                .map(Flag)
                .ok_or_else(|| de::Error::custom(format!("must be true or false, got \"{s}\""))),
            other => Err(de::Error::custom(format!("must be true or false, got {other}"))),
        }
    }
}

impl JsonSchema for Count {
    fn schema_name() -> Cow<'static, str> {
        "Count".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"anyOf": [{"type": "integer", "minimum": 0}, {"type": "string", "format": COUNT_FORMAT}]})
    }

    fn inline_schema() -> bool {
        true
    }
}

impl JsonSchema for BlankableCount {
    fn schema_name() -> Cow<'static, str> {
        "BlankableCount".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"anyOf": [{"type": "integer", "minimum": 0}, {"type": "string", "format": BLANKABLE_COUNT_FORMAT}, {"type": "null"}]})
    }

    fn inline_schema() -> bool {
        true
    }
}

impl JsonSchema for Decimal {
    fn schema_name() -> Cow<'static, str> {
        "Decimal".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"anyOf": [{"type": "number"}, {"type": "string", "format": DECIMAL_FORMAT}]})
    }

    fn inline_schema() -> bool {
        true
    }
}

impl JsonSchema for Flag {
    fn schema_name() -> Cow<'static, str> {
        "Flag".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"anyOf": [{"type": "boolean"}, {"type": "string", "format": FLAG_FORMAT}]})
    }

    fn inline_schema() -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_count_reads_its_number_and_its_text() {
        assert_eq!(serde_json::from_value::<Count>(json!(3)).unwrap(), Count(3));
        assert_eq!(serde_json::from_value::<Count>(json!(" 3 ")).unwrap(), Count(3));
        assert!(serde_json::from_value::<Count>(json!("three")).is_err());
        assert!(serde_json::from_value::<Count>(json!(-1)).is_err());
        assert!(serde_json::from_value::<Count>(json!("")).is_err());
    }

    #[test]
    fn a_blankable_count_reads_blank_and_null_as_unset() {
        for unset in [json!(null), json!(""), json!("  ")] {
            assert_eq!(serde_json::from_value::<BlankableCount>(unset).unwrap(), BlankableCount(None));
        }
        assert_eq!(serde_json::from_value::<BlankableCount>(json!("45")).unwrap(), BlankableCount(Some(Count(45))));
        assert!(serde_json::from_value::<BlankableCount>(json!("soon")).is_err());
    }

    #[test]
    fn a_flag_reads_its_boolean_and_its_text() {
        assert_eq!(serde_json::from_value::<Flag>(json!(true)).unwrap(), Flag(true));
        assert_eq!(serde_json::from_value::<Flag>(json!("On")).unwrap(), Flag(true));
        assert_eq!(serde_json::from_value::<Flag>(json!("")).unwrap(), Flag(false));
        assert!(serde_json::from_value::<Flag>(json!("maybe")).is_err());
    }

    #[test]
    fn only_a_whole_string_reference_is_a_placeholder() {
        for whole in ["{{n}}", "{{ n }}", "{{item.id}}", "{{from.output}}"] {
            assert!(is_placeholder(whole), "{whole:?}");
        }
        for not in ["n={{n}}", "{{a}}{{b}}", "{{a}} {{b}}", "x{{n}}", "{{n}}x", "{{", "{{}}x}}", "3"] {
            assert!(!is_placeholder(not), "{not:?}");
        }
        assert_eq!(whole_placeholder("{{ workspace }}"), Some("workspace"));
        assert_eq!(whole_placeholder("{{a}}{{b}}"), None);
    }

    #[test]
    fn a_decimal_reads_its_number_and_its_text() {
        assert_eq!(serde_json::from_value::<Decimal>(json!(0.5)).unwrap(), Decimal(0.5));
        assert_eq!(serde_json::from_value::<Decimal>(json!(1)).unwrap(), Decimal(1.0));
        assert_eq!(serde_json::from_value::<Decimal>(json!(" 0.25 ")).unwrap(), Decimal(0.25));
        assert!(serde_json::from_value::<Decimal>(json!("warm")).is_err());
        assert!(serde_json::from_value::<Decimal>(json!("")).is_err());
        assert!(serde_json::from_value::<Decimal>(json!("NaN")).is_err());
        assert!(serde_json::from_value::<Decimal>(json!(true)).is_err());
    }

    #[test]
    fn the_gate_and_the_load_agree_on_every_text_form() {
        for text in ["3", " 3 ", "three", "", "-1", "{{draws}}", "n={{n}}"] {
            let gate = text_form_ok(COUNT_FORMAT, text).unwrap();
            let load = serde_json::from_value::<Count>(json!(text)).is_ok();
            assert_eq!(gate, load || is_placeholder(text), "count {text:?}");
            let gate = text_form_ok(BLANKABLE_COUNT_FORMAT, text).unwrap();
            let load = serde_json::from_value::<BlankableCount>(json!(text)).is_ok();
            assert_eq!(gate, load || is_placeholder(text), "blankable count {text:?}");
        }
        for text in ["true", "OFF", "1", "", "maybe", "{{no_fetch}}"] {
            let gate = text_form_ok(FLAG_FORMAT, text).unwrap();
            let load = serde_json::from_value::<Flag>(json!(text)).is_ok();
            assert_eq!(gate, load || is_placeholder(text), "flag {text:?}");
        }
        for text in ["0.5", " 1 ", "hot", "", "{{temp}}", "t={{temp}}", "inf"] {
            let gate = text_form_ok(DECIMAL_FORMAT, text).unwrap();
            let load = serde_json::from_value::<Decimal>(json!(text)).is_ok();
            assert_eq!(gate, load || is_placeholder(text), "decimal {text:?}");
        }
        assert_eq!(text_form_ok("uri", "x"), None);
    }
}
