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
//! - a string, number, boolean or null: equal to the reply's value;
//! - `{"anyOf": [v, ...]}`: any one of the listed values;
//! - `{"includes": [v, ...]}`: a list holding each listed item, extras allowed;
//! - a plain list: a list holding exactly these items, in any order;
//! - any other object: an object whose listed keys each match (keys the
//!   expected object does not list are not checked).
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
        self.unreadable.is_none() && self.missed.is_empty()
    }

    pub(crate) fn details(&self) -> String {
        if let Some(why) = &self.unreadable {
            return format!("fields 0/{}: {why}", self.expected);
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

/// The first JSON object in a reply. A ```json fence around it, or prose
/// before it, is tolerated; what follows the object is not read.
fn first_object(reply: &str) -> Result<Map<String, Value>, String> {
    let mut rest = reply;
    while let Some(start) = rest.find('{') {
        let candidate = &rest[start..];
        let mut stream = serde_json::Deserializer::from_str(candidate).into_iter::<Value>();
        if let Some(Ok(Value::Object(obj))) = stream.next() {
            return Ok(obj);
        }
        rest = &candidate[1..];
    }
    Err("no JSON object in the reply".to_string())
}

fn matches(want: &Value, got: Option<&Value>) -> bool {
    if let Some(alts) = keyword_list(want, "anyOf") {
        return alts.iter().any(|alt| matches(alt, got));
    }
    if let Some(items) = keyword_list(want, "includes") {
        let Some(Value::Array(have)) = got else {
            return false;
        };
        return items
            .iter()
            .all(|item| have.iter().any(|h| matches(item, Some(h))));
    }
    let Some(got) = got else { return false };
    match (want, got) {
        (Value::String(w), Value::String(g)) => w.trim() == g.trim(),
        (Value::Array(w), Value::Array(g)) => {
            w.len() == g.len() && pair_off(w, g, &mut vec![false; g.len()])
        }
        (Value::Object(w), Value::Object(g)) => w.iter().all(|(k, v)| matches(v, g.get(k))),
        (Value::Array(_) | Value::Object(_) | Value::String(_), _) => false,
        (w, g) => w == g,
    }
}

/// `{"<keyword>": [...]}` and nothing else.
fn keyword_list<'a>(want: &'a Value, keyword: &str) -> Option<&'a Vec<Value>> {
    match want {
        Value::Object(o) if o.len() == 1 => o.get(keyword)?.as_array(),
        _ => None,
    }
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
        // A brace in prose that does not open an object is skipped.
        assert!(grade(&e, "Sure {not json} then {\"a\": \"1\"}").passed());
    }

    #[test]
    fn a_reply_with_no_json_object_fails_every_field_and_says_why() {
        let e = expect(json!({"a": "1", "b": "2"}));
        for reply in ["", "no json here", "[1, 2]", "{\"a\": "] {
            let r = grade(&e, reply);
            assert!(!r.passed(), "{reply:?}");
            assert_eq!((r.correct, r.expected), (0, 2));
            assert_eq!(r.details(), "fields 0/2: no JSON object in the reply");
        }
    }
}
