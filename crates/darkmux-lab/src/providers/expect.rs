//! The structured-output check of a `prompt` workload: `verify.expect`.
//!
//! The workload names the answer it expects, field by field, and the reply's
//! first JSON object is compared against it. darkmux makes no assumption
//! about what a value means: strings compare exactly (case, punctuation and
//! inner spacing all count), with only leading and trailing whitespace
//! trimmed. Where more than one answer is right (a name with or without
//! "LLC", a phone written two ways on the page), the workload lists each one;
//! darkmux never decides that two spellings are the same.
//!
//! An expected value is one of:
//! - a string, boolean or null: equal to the reply's value;
//! - a number: the same number (`1` and `1.0` are one value; an integer past
//!   2^53 is not the float it rounds to);
//! - `{"anyOf": [v, ...]}`: any one of the listed values;
//! - `{"includes": [v, ...]}`: a list holding each listed item, extras allowed;
//! - `{"items": [v, ...], "optional": [v, ...]}`: a list holding each of
//!   `items`, and otherwise only entries of `optional` (each at most once):
//!   answers that are true but were not asked for, such as a free offer
//!   listed among prices;
//! - a plain list: a list holding exactly these items, in any order;
//! - any other object: an object whose listed keys each match (keys the
//!   expected object does not list are not checked).
//!
//! In `includes` and `items`/`optional`, each entry takes its own reply item,
//! so one reply item never satisfies two entries.
//!
//! A one-key object named `anyOf` or `includes`, and a two-key object of
//! `items` and `optional`, are always read as the keyword, so fields of those
//! names cannot be expected inside a value.
//!
//! The verdict is all or nothing (every top-level field matches); the count
//! of fields right out of fields expected is recorded beside it, the same
//! measure for every structured-output workload.

use serde_json::{Map, Value};

/// How a reply fared against `verify.expect`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FieldsReport {
    pub correct: usize,
    pub expected: usize,
    /// The top-level fields that did not match, in the workload's order.
    pub missed: Vec<String>,
    /// Why nothing could be compared (no JSON object in the reply), if so.
    pub unreadable: Option<String>,
}

impl FieldsReport {
    pub(crate) fn passed(&self) -> bool {
        // An empty expectation checks nothing, so it never reads as a pass.
        self.unreadable.is_none() && self.missed.is_empty() && self.expected > 0
    }

    pub(crate) fn details(&self) -> String {
        if let Some(why) = &self.unreadable {
            return format!("fields 0/{}: {why}", self.expected);
        }
        if self.expected == 0 {
            return "fields 0/0: verify.expect names no field".to_string();
        }
        if self.missed.is_empty() {
            return format!("fields {}/{}: all match", self.correct, self.expected);
        }
        format!(
            "fields {}/{}: missed {}",
            self.correct,
            self.expected,
            self.missed.join(", ")
        )
    }
}

/// Compare the reply's first JSON object against `expect`.
pub(crate) fn grade(expect: &Map<String, Value>, reply: &str) -> FieldsReport {
    let expected = expect.len();
    let answer = match first_object(reply) {
        Ok(a) => a,
        Err(why) => {
            return FieldsReport {
                correct: 0,
                expected,
                missed: expect.keys().cloned().collect(),
                unreadable: Some(why),
            }
        }
    };
    let missed: Vec<String> = expect
        .iter()
        .filter(|(k, want)| !matches(want, answer.get(k.as_str())))
        .map(|(k, _)| k.clone())
        .collect();
    FieldsReport {
        correct: expected - missed.len(),
        expected,
        missed,
        unreadable: None,
    }
}

/// The first JSON object in the reply that parses. A ```json fence or prose
/// before it is tolerated, including a `{` that is not itself JSON. A `{`
/// that begins an object cut off mid-parse is the reply's answer, and it has
/// none: an object nested inside it is never graded in its place. What
/// follows a parsed object is not read.
fn first_object(reply: &str) -> Result<Map<String, Value>, String> {
    let mut rest = reply;
    loop {
        let Some(rel) = rest.find('{') else {
            return Err("no JSON object in the reply".to_string());
        };
        // `{` is one byte, and `find` returns a char boundary.
        let candidate = &rest[rel..];
        let mut stream = serde_json::Deserializer::from_str(candidate).into_iter::<Value>();
        match stream.next() {
            Some(Ok(Value::Object(obj))) => return Ok(obj),
            Some(Err(e)) if e.is_eof() => {
                return Err("the reply's JSON object does not parse (cut off?)".to_string());
            }
            _ => rest = &candidate['{'.len_utf8()..],
        }
    }
}

fn matches(want: &Value, got: Option<&Value>) -> bool {
    if let Some(verdict) = keyword_match(want, got) {
        return verdict;
    }
    let Some(got) = got else { return false };
    match (want, got) {
        (Value::String(w), Value::String(g)) => w.trim() == g.trim(),
        (Value::Array(w), Value::Array(g)) => {
            w.len() == g.len() && pair_off(w, g, &mut vec![false; g.len()])
        }
        (Value::Object(w), Value::Object(g)) => w.iter().all(|(k, v)| matches(v, g.get(k))),
        (Value::Number(w), Value::Number(g)) => numbers_equal(w, g),
        (Value::Array(_) | Value::Object(_) | Value::String(_) | Value::Number(_), _) => false,
        (w, g) => w == g,
    }
}

/// Two JSON numbers are the same value. Integers compare as integers, so
/// values past 2^53 (where `f64` can no longer tell neighbors apart) stay
/// distinct. A float matches an integer only when the integer is exactly
/// that float (`1` and `1.0`, `0.5` and `5e-1`).
fn numbers_equal(want: &serde_json::Number, got: &serde_json::Number) -> bool {
    if let (Some(a), Some(b)) = (want.as_i64(), got.as_i64()) {
        return a == b;
    }
    if let (Some(a), Some(b)) = (want.as_u64(), got.as_u64()) {
        return a == b;
    }
    match (exact_f64(want), exact_f64(got)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// The number as an `f64` only when that `f64` is exactly the same value.
/// An integer that does not round-trip through `f64` is `None`, so it cannot
/// compare equal to the float it collapses into. `as i64` saturates, so the
/// check uses a wider integer: `i64::MAX as f64 as i64` is `i64::MAX` even
/// though the float is 2^63.
fn exact_f64(n: &serde_json::Number) -> Option<f64> {
    if let Some(i) = n.as_i64() {
        let f = i as f64;
        return (f as i128 == i128::from(i)).then_some(f);
    }
    if let Some(u) = n.as_u64() {
        let f = u as f64;
        return (f as u128 == u128::from(u)).then_some(f);
    }
    n.as_f64().filter(|f| f.is_finite())
}

/// The verdict of an expected value written as a keyword (`anyOf`,
/// `includes`, `items`/`optional`); `None` for any other value.
fn keyword_match(want: &Value, got: Option<&Value>) -> Option<bool> {
    if let Some(alts) = keyword_list(want, "anyOf") {
        return Some(alts.iter().any(|alt| matches(alt, got)));
    }
    let have = match got {
        Some(Value::Array(have)) => Some(have),
        _ => None,
    };
    if let Some(items) = keyword_list(want, "includes") {
        return Some(have.is_some_and(|have| pair_off(items, have, &mut vec![false; have.len()])));
    }
    let (items, optional) = items_and_optional(want)?;
    Some(have.is_some_and(|have| pair_with_optional(items, optional, have, &mut vec![false; have.len()])))
}

/// `{"<keyword>": [...]}` and nothing else.
fn keyword_list<'a>(want: &'a Value, keyword: &str) -> Option<&'a Vec<Value>> {
    match want {
        Value::Object(o) if o.len() == 1 => o.get(keyword)?.as_array(),
        _ => None,
    }
}

/// `{"items": [...], "optional": [...]}` and nothing else.
fn items_and_optional(want: &Value) -> Option<(&Vec<Value>, &Vec<Value>)> {
    match want {
        Value::Object(o) if o.len() == 2 => Some((o.get("items")?.as_array()?, o.get("optional")?.as_array()?)),
        _ => None,
    }
}

/// Each required item matched to its own reply item, then every reply item
/// left over matched to its own optional entry. Backtracks over the required
/// pairing too: a reply item a required `anyOf` took may be the only one an
/// optional entry could cover.
fn pair_with_optional(items: &[Value], optional: &[Value], got: &[Value], used: &mut Vec<bool>) -> bool {
    let Some((first, rest)) = items.split_first() else {
        let left: Vec<&Value> = got.iter().zip(used.iter()).filter(|(_, u)| !**u).map(|(g, _)| g).collect();
        return cover(&left, optional, &mut vec![false; optional.len()]);
    };
    for i in 0..got.len() {
        if !used[i] && matches(first, Some(&got[i])) {
            used[i] = true;
            if pair_with_optional(rest, optional, got, used) {
                return true;
            }
            used[i] = false;
        }
    }
    false
}

/// Whether each reply item can be matched to its own optional entry.
fn cover(left: &[&Value], optional: &[Value], taken: &mut Vec<bool>) -> bool {
    let Some((first, rest)) = left.split_first() else {
        return true;
    };
    for j in 0..optional.len() {
        if !taken[j] && matches(&optional[j], Some(first)) {
            taken[j] = true;
            if cover(rest, optional, taken) {
                return true;
            }
            taken[j] = false;
        }
    }
    false
}

/// Whether each expected item can be matched to its own reply item. Small
/// lists, so a plain backtracking search: with `anyOf` items a greedy pairing
/// could take the one reply item a later expected item needed.
fn pair_off(want: &[Value], got: &[Value], used: &mut Vec<bool>) -> bool {
    let Some((first, rest)) = want.split_first() else {
        return true;
    };
    for i in 0..got.len() {
        if !used[i] && matches(first, Some(&got[i])) {
            used[i] = true;
            if pair_off(rest, got, used) {
                return true;
            }
            used[i] = false;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn expect(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn strings_compare_exactly_after_trimming_the_ends() {
        let e = expect(json!({"name": "Copperleaf Cycle Works"}));
        assert!(grade(&e, r#"{"name": "  Copperleaf Cycle Works\n"}"#).passed());
        // Case, punctuation and inner spacing all count.
        for off in [
            "copperleaf cycle works",
            "Copperleaf Cycle Works.",
            "Copperleaf  Cycle Works",
            "Copperleaf Cycle-Works",
        ] {
            let r = grade(&e, &json!({ "name": off }).to_string());
            assert_eq!(
                (r.passed(), r.missed.clone()),
                (false, vec!["name".to_string()]),
                "{off:?} must not match"
            );
        }
    }

    #[test]
    fn any_of_accepts_each_listed_answer_and_nothing_else() {
        let e = expect(json!({"phone": {"anyOf": ["(541) 555-0142", "541-555-0142"]}}));
        assert!(grade(&e, r#"{"phone": "(541) 555-0142"}"#).passed());
        assert!(grade(&e, r#"{"phone": "541-555-0142"}"#).passed());
        assert!(!grade(&e, r#"{"phone": "541.555.0142"}"#).passed());
        assert!(!grade(&e, r#"{"phone": "5415550142"}"#).passed());
    }

    #[test]
    fn a_plain_list_is_exactly_these_items_in_any_order() {
        let e = expect(json!({"prices": ["$79", "$189"]}));
        assert!(grade(&e, r#"{"prices": ["$189", "$79"]}"#).passed());
        assert!(
            !grade(&e, r#"{"prices": ["$79"]}"#).passed(),
            "a missing item fails"
        );
        assert!(
            !grade(&e, r#"{"prices": ["$79", "$189", "Free"]}"#).passed(),
            "an extra item fails"
        );
        assert!(
            !grade(&e, r#"{"prices": ["$79", "$79"]}"#).passed(),
            "each expected item needs its own reply item"
        );
        assert!(
            !grade(&e, r#"{"prices": "$79, $189"}"#).passed(),
            "a string is not a list"
        );
    }

    #[test]
    fn includes_allows_extras_but_needs_every_listed_item() {
        let e = expect(json!({"services": {"includes": ["Tune-up", "Flat repair"]}}));
        assert!(grade(
            &e,
            r#"{"services": ["Flat repair", "Tune-up", "Bike fit"]}"#
        )
        .passed());
        assert!(!grade(&e, r#"{"services": ["Tune-up"]}"#).passed());
    }

    #[test]
    fn includes_needs_its_own_reply_item_for_each_listed_item() {
        let e = expect(json!({"services": {"includes": ["Tune-ups", {"anyOf": ["Basic tune-up", "Tune-ups"]}]}}));
        assert!(!grade(&e, r#"{"services": ["Tune-ups"]}"#).passed());
        assert!(grade(&e, r#"{"services": ["Tune-ups", "Basic tune-up", "Bike fit"]}"#).passed());
        let twice = expect(json!({"x": {"includes": ["a", "a"]}}));
        assert!(!grade(&twice, r#"{"x": ["a"]}"#).passed());
        assert!(grade(&twice, r#"{"x": ["a", "b", "a"]}"#).passed());
    }

    #[test]
    fn optional_items_may_appear_and_nothing_else_may() {
        let e = expect(json!({"prices": {
            "items": [{"price": "$79"}, {"price": "$15"}],
            "optional": [{"price": "Free"}, {"price": {"anyOf": ["Call for a quote", "Varies"]}}]
        }}));
        // The required items alone, in any order.
        assert!(grade(&e, r#"{"prices": [{"price": "$15"}, {"price": "$79"}]}"#).passed());
        // Plus any of the optional ones.
        assert!(grade(&e, r#"{"prices": [{"price": "$79"}, {"price": "Free"}, {"price": "$15"}, {"price": "Varies"}]}"#).passed());
        // An item that is neither fails the field.
        assert!(!grade(&e, r#"{"prices": [{"price": "$79"}, {"price": "$15"}, {"price": "$5"}]}"#).passed());
        // A missing required item fails it, however many optional ones are there.
        assert!(!grade(&e, r#"{"prices": [{"price": "$79"}, {"price": "Free"}]}"#).passed());
        // Each optional entry is used once: a repeat is an item it does not cover.
        assert!(!grade(&e, r#"{"prices": [{"price": "$79"}, {"price": "$15"}, {"price": "Free"}, {"price": "Free"}]}"#).passed());
        assert!(!grade(&e, r#"{"prices": "Free"}"#).passed());
    }

    #[test]
    fn optional_pairing_backtracks_over_the_required_items_too() {
        // "a" must go to the optional entry, "b" to the required anyOf.
        let e = expect(json!({"x": {"items": [{"anyOf": ["a", "b"]}], "optional": ["a"]}}));
        assert!(grade(&e, r#"{"x": ["a", "b"]}"#).passed());
        // A reply item a failed try took is free again, so it still counts.
        let none = expect(json!({"x": {"items": [{"anyOf": ["a", "b"]}], "optional": []}}));
        assert!(!grade(&none, r#"{"x": ["a", "b"]}"#).passed());
        // Leftover items backtrack over the optional entries as well.
        let opt = expect(json!({"x": {"items": [], "optional": [{"anyOf": ["a", "b"]}, "a"]}}));
        assert!(grade(&opt, r#"{"x": ["a", "b"]}"#).passed());
    }

    #[test]
    fn list_items_can_list_their_own_accepted_spellings() {
        // Case is the fixture's call, made in its data: here either casing counts.
        let e = expect(json!({"services": [{"anyOf": ["Tune-up", "tune-up"]}, "Flat repair"]}));
        assert!(grade(&e, r#"{"services": ["tune-up", "Flat repair"]}"#).passed());
        assert!(!grade(&e, r#"{"services": ["TUNE-UP", "Flat repair"]}"#).passed());
    }

    #[test]
    fn pairing_backtracks_when_a_greedy_match_would_take_the_wrong_item() {
        let e = expect(json!({"x": [{"anyOf": ["a", "b"]}, "a"]}));
        assert!(grade(&e, r#"{"x": ["a", "b"]}"#).passed());
    }

    #[test]
    fn nested_objects_check_only_the_keys_they_list() {
        let e = expect(json!({"prices": [{"item": "Standard tune-up", "price": "$79"}]}));
        assert!(grade(
            &e,
            r#"{"prices": [{"item": "Standard tune-up", "price": "$79", "page": "services"}]}"#
        )
        .passed());
        assert!(!grade(
            &e,
            r#"{"prices": [{"item": "Standard tune-up", "price": "$79.00"}]}"#
        )
        .passed());
        assert!(
            !grade(&e, r#"{"prices": [{"item": "Standard tune-up"}]}"#).passed(),
            "a missing key fails"
        );
    }

    #[test]
    fn numbers_booleans_and_null_compare_as_json_values() {
        let e = expect(json!({"n": 5, "ok": true, "none": null}));
        assert!(grade(&e, r#"{"n": 5, "ok": true, "none": null}"#).passed());
        let r = grade(&e, r#"{"n": "5", "ok": "true"}"#);
        assert_eq!(r.missed, vec!["n", "ok", "none"], "in the workload's order");
        // null is a value: a missing key is not null.
        assert_eq!(
            grade(&expect(json!({"none": null})), "{}").missed,
            vec!["none"]
        );
        // A number is its value, however the JSON spells it, when that
        // spelling is exact. `1` and `1.0` are one value; so are `0.5` and
        // `5e-1`.
        let one = expect(json!({"n": 1}));
        assert!(grade(&one, r#"{"n": 1.0}"#).passed());
        assert!(grade(&expect(json!({"n": 1.0})), r#"{"n": 1}"#).passed());
        assert!(!grade(&one, r#"{"n": 1.5}"#).passed());
        assert!(grade(&expect(json!({"n": 0.5})), r#"{"n": 5e-1}"#).passed());
    }

    /// Past 2^53 a JSON integer is not the f64 it rounds to. Two integers
    /// one apart must not compare equal, and neither equals the float the
    /// other collapses into.
    #[test]
    fn integers_past_the_f64_mantissa_stay_distinct() {
        let big = "9007199254740993"; // 2^53 + 1
        let prev = "9007199254740992"; // 2^53
        let e = expect(json!({"n": 9007199254740993_i64}));
        assert!(grade(&e, &format!(r#"{{"n": {big}}}"#)).passed(), "the same integer matches");
        assert!(!grade(&e, &format!(r#"{{"n": {prev}}}"#)).passed(), "one apart is not equal");
        assert!(!grade(&e, &format!(r#"{{"n": {prev}.0}}"#)).passed(), "the float 2^53 is not the integer 2^53+1");
        // 2^53 itself is exactly a float, so the two spellings are one value.
        assert!(grade(&expect(json!({"n": 9007199254740992_i64})), r#"{"n": 9007199254740992.0}"#).passed());
        // `as i64` saturates, which would make i64::MAX look equal to 2^63.
        assert!(!grade(&expect(json!({"n": 9223372036854775807_i64})), r#"{"n": 9223372036854775808.0}"#).passed());
        assert!(grade(&expect(json!({"n": 9223372036854775807_i64})), r#"{"n": 9223372036854775807}"#).passed());
        let huge = "18446744073709551615"; // u64::MAX
        let one_less = "18446744073709551614";
        let u = expect(json!({"n": 18446744073709551615_u64}));
        assert!(grade(&u, &format!(r#"{{"n": {huge}}}"#)).passed());
        assert!(!grade(&u, &format!(r#"{{"n": {one_less}}}"#)).passed());
        // Same saturation on the u64 side: u64::MAX's f64 is 2^64.
        assert!(!grade(&u, r#"{"n": 18446744073709551616.0}"#).passed());
    }

    #[test]
    fn keys_the_expectation_does_not_name_are_not_checked() {
        let e = expect(json!({"email": "hello@copperleaf-cycles.example"}));
        assert!(grade(
            &e,
            r#"{"email": "hello@copperleaf-cycles.example", "summary": "anything", "clarity": 4}"#
        )
        .passed());
    }

    #[test]
    fn the_count_is_fields_right_out_of_fields_expected() {
        let e = expect(json!({"a": "1", "b": "2", "c": "3", "d": "4"}));
        let r = grade(&e, r#"{"a": "1", "b": "x", "c": "3"}"#);
        assert_eq!(
            (r.correct, r.expected, r.missed.clone()),
            (2, 4, vec!["b".to_string(), "d".to_string()])
        );
        assert!(!r.passed());
        assert_eq!(r.details(), "fields 2/4: missed b, d");
        let all = grade(&e, r#"{"a": "1", "b": "2", "c": "3", "d": "4"}"#);
        assert_eq!(
            (all.passed(), all.details()),
            (true, "fields 4/4: all match".to_string())
        );
    }

    #[test]
    fn the_first_json_object_is_read_through_a_fence_or_prose() {
        let e = expect(json!({"a": "1"}));
        assert!(grade(&e, "```json\n{\"a\": \"1\"}\n```").passed());
        assert!(grade(
            &e,
            "Here is the answer: {\"a\": \"1\"} hope that helps {\"a\": \"2\"}"
        )
        .passed());
    }

    #[test]
    fn a_reply_with_no_json_object_fails_every_field_and_says_why() {
        let e = expect(json!({"a": "1", "b": "2"}));
        for reply in ["", "no json here", "[1, 2]"] {
            let r = grade(&e, reply);
            assert!(!r.passed(), "{reply:?}");
            assert_eq!((r.correct, r.expected), (0, 2));
            assert_eq!(r.details(), "fields 0/2: no JSON object in the reply");
        }
    }

    /// A reply cut off mid-object has no answer, even when an object nested
    /// inside it would match on its own. A `{` that is not JSON is not that
    /// object: the reply's answer is the first object that parses.
    #[test]
    fn a_cut_off_reply_is_never_graded_by_an_object_nested_inside_it() {
        let e = expect(json!({"a": "1", "b": "2"}));
        for reply in [
            r#"{"items":[{"a":"1","b":"2"}, {"a""#,
            r#"{"a": "#,
            r#"Sure {not json} then {"a": "#,
        ] {
            let r = grade(&e, reply);
            assert!(!r.passed(), "{reply:?}");
            assert_eq!(r.details(), "fields 0/2: the reply's JSON object does not parse (cut off?)", "{reply:?}");
        }
    }

    #[test]
    fn a_brace_that_is_not_json_does_not_hide_the_answer_after_it() {
        let e = expect(json!({"a": "1", "b": "2"}));
        assert!(grade(&e, r#"Sure {not json} then {"a": "1", "b": "2"}"#).passed());
        assert!(!grade(&expect(json!({"a": "1"})), "{not json}").passed());
    }

    #[test]
    fn an_empty_expectation_is_never_a_pass() {
        let r = grade(&Map::new(), r#"{"a": "1"}"#);
        assert!(!r.passed());
        assert_eq!(r.details(), "fields 0/0: verify.expect names no field");
        assert_eq!((r.correct, r.expected), (0, 0));
    }
}
