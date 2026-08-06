//! The value type the script-state store holds.
//!
//! This used to be `serde_json::Value`, which was convenient right up until the
//! wire stopped being JSON. `Value`'s `Deserialize` impl works by calling
//! `deserialize_any` — it asks the format "what is the next thing?" — and only a
//! *self-describing* format can answer that. postcard, like every compact binary
//! format, is not: it writes a `7` with no indication of whether that was an
//! integer, a string length, or an enum discriminant, because the schema on
//! both ends is expected to say. So a `Value` simply cannot be decoded from it.
//!
//! [`ScriptValue`] is the same shape with the tag written down. The derived
//! `Deserialize` knows which variant it is looking at from the discriminant the
//! derived `Serialize` wrote, so nothing has to be inferred from the bytes.
//!
//! JSON does not disappear — it is still what the CLI prints and what the store
//! is saved as — it just stops being what two processes speak to each other.
//! [`From`] conversions in both directions are what keep those edges working.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Any value a script can store.
///
/// [`Int`](ScriptValue::Int) and [`Float`](ScriptValue::Float) are deliberately
/// separate rather than one `f64`. Lua distinguishes them, JSON round trips them
/// differently, and a window ID past 2^53 silently loses precision if everything
/// is a float — which is exactly the kind of value a script keeps in here.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ScriptValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<ScriptValue>),
    /// Sorted, so a store saved to disk is stable and diffable rather than
    /// reshuffling on every write.
    Map(BTreeMap<String, ScriptValue>),
}

/// `Eq` by hand because `f64` is not `Eq` — but every *stored* value is one a
/// script wrote and can compare against, and compare-and-set needs equality to
/// mean something. Two `Float`s are equal when their bits are, which makes
/// `NaN == NaN` true here. That is the useful answer for "is this still what I
/// last wrote?", and the only case where it differs from IEEE equality.
impl Eq for ScriptValue {}

impl ScriptValue {
    /// The value as a string, when it is one.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(value) => Some(value),
            _ => None,
        }
    }

    /// Whether this is [`Null`](ScriptValue::Null).
    #[must_use]
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }
}

impl From<serde_json::Value> for ScriptValue {
    fn from(value: serde_json::Value) -> Self {
        match value {
            serde_json::Value::Null => Self::Null,
            serde_json::Value::Bool(value) => Self::Bool(value),
            serde_json::Value::Number(number) => number.as_i64().map_or_else(
                // A JSON number that is not an `i64` is either a float or an
                // integer too large to be one; both are best kept as a float,
                // and `as_f64` is infallible for every other case.
                || Self::Float(number.as_f64().unwrap_or(f64::NAN)),
                Self::Int,
            ),
            serde_json::Value::String(value) => Self::Str(value),
            serde_json::Value::Array(values) => {
                Self::List(values.into_iter().map(Self::from).collect())
            }
            serde_json::Value::Object(entries) => Self::Map(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, Self::from(value)))
                    .collect(),
            ),
        }
    }
}

impl From<ScriptValue> for serde_json::Value {
    fn from(value: ScriptValue) -> Self {
        match value {
            ScriptValue::Null => Self::Null,
            ScriptValue::Bool(value) => Self::Bool(value),
            ScriptValue::Int(value) => Self::Number(value.into()),
            // A non-finite float has no JSON spelling at all, so it renders as
            // null rather than producing a document nothing can parse.
            ScriptValue::Float(value) => {
                serde_json::Number::from_f64(value).map_or(Self::Null, Self::Number)
            }
            ScriptValue::Str(value) => Self::String(value),
            ScriptValue::List(values) => Self::Array(values.into_iter().map(Self::from).collect()),
            ScriptValue::Map(entries) => Self::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, Self::from(value)))
                    .collect(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The property the whole type exists for: it survives a non-self-describing
    /// format, which `serde_json::Value` cannot.
    #[test]
    fn every_shape_survives_postcard() {
        let value = ScriptValue::Map(BTreeMap::from([
            ("null".to_string(), ScriptValue::Null),
            ("bool".to_string(), ScriptValue::Bool(true)),
            ("int".to_string(), ScriptValue::Int(-42)),
            ("float".to_string(), ScriptValue::Float(1.5)),
            ("str".to_string(), ScriptValue::Str("hello".to_string())),
            (
                "list".to_string(),
                ScriptValue::List(vec![ScriptValue::Int(1), ScriptValue::Str("two".into())]),
            ),
        ]));

        let bytes = postcard::to_allocvec(&value).expect("encodes");
        let decoded: ScriptValue = postcard::from_bytes(&bytes).expect("decodes");
        assert_eq!(decoded, value);
    }

    #[test]
    fn json_round_trips_through_the_store_type() {
        let original = json!({
            "pads": {"term": {"window": 4_611_686_018_427_387_904_i64, "open": true}},
            "ratio": 0.25,
            "names": ["a", "b"],
            "nothing": null,
        });

        let stored = ScriptValue::from(original.clone());
        let back = serde_json::Value::from(stored);
        assert_eq!(back, original);
    }

    /// The reason `Int` and `Float` are separate variants: a window ID past
    /// 2^53 is not representable as an `f64`, and a script keeps exactly those.
    #[test]
    fn a_large_integer_keeps_every_digit() {
        let id = 9_007_199_254_740_993_i64; // 2^53 + 1
        let stored = ScriptValue::from(json!(id));
        assert_eq!(stored, ScriptValue::Int(id));

        let bytes = postcard::to_allocvec(&stored).expect("encodes");
        let decoded: ScriptValue = postcard::from_bytes(&bytes).expect("decodes");
        assert_eq!(decoded, ScriptValue::Int(id));
        assert_eq!(serde_json::Value::from(decoded), json!(id));
    }

    /// Non-finite floats have no JSON spelling; they must not produce a document
    /// that nothing can parse.
    #[test]
    fn a_non_finite_float_renders_as_null() {
        assert_eq!(
            serde_json::Value::from(ScriptValue::Float(f64::INFINITY)),
            serde_json::Value::Null
        );
    }
}
