//! Rendering wire types as the JSON a terminal sees.
//!
//! Several types are documented as JSON objects carrying a discriminant field —
//! `{"event": "window_focused", …}`, `{"outcome": "applied", …}` — which serde
//! spells `#[serde(tag = "…")]`. That representation cannot be *decoded* from a
//! binary format: it works by reading the whole value and then looking for the
//! tag inside it, which requires asking the format what each piece is, and only
//! a self-describing format can answer.
//!
//! So the types derive the ordinary externally tagged form, which a binary
//! format handles, and this is what turns that into the documented shape on the
//! way out. The JSON a client sees is unchanged; it is simply produced here
//! rather than by an attribute.

/// Rewrites serde's externally tagged `{"variant": {…}}` into the flat
/// `{"tag": "variant", …}` clients are documented to read.
///
/// A variant with no fields becomes just `{"tag": "variant"}`, and a variant
/// with an unnamed payload keeps it under `value`, since there is no field name
/// to flatten it into.
#[must_use]
pub fn flatten_tag(value: serde_json::Value, tag: &str) -> serde_json::Value {
    let serde_json::Value::Object(outer) = value else {
        // A unit-only enum serialises as a bare string, which is already as flat
        // as it gets.
        return value;
    };

    let mut entries = outer.into_iter();
    let (Some((name, payload)), None) = (entries.next(), entries.next()) else {
        // More than one key means this was not an externally tagged enum after
        // all; pass it through rather than mangling it.
        return serde_json::Value::Object(entries.collect());
    };

    let mut flat = serde_json::Map::new();
    flat.insert(tag.to_string(), serde_json::Value::String(name));
    match payload {
        serde_json::Value::Object(fields) => flat.extend(fields),
        serde_json::Value::Null => {}
        other => {
            flat.insert("value".to_string(), other);
        }
    }
    serde_json::Value::Object(flat)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_struct_variant_flattens_into_the_tag() {
        assert_eq!(
            flatten_tag(json!({"window_focused": {"window_id": 4}}), "event"),
            json!({"event": "window_focused", "window_id": 4})
        );
    }

    #[test]
    fn a_unit_variant_is_just_its_tag() {
        assert_eq!(
            flatten_tag(json!("applied"), "outcome"),
            json!("applied")
        );
        assert_eq!(
            flatten_tag(json!({"applied": null}), "outcome"),
            json!({"outcome": "applied"})
        );
    }

    #[test]
    fn an_unnamed_payload_keeps_a_name() {
        assert_eq!(
            flatten_tag(json!({"conflict": 7}), "outcome"),
            json!({"outcome": "conflict", "value": 7})
        );
    }
}
