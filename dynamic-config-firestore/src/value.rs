//! Firestore's value encoding, turned back into ordinary JSON.
//!
//! Firestore does not store JSON. It stores a tagged encoding —
//! `{"port": {"integerValue": "5432"}}` — where every value names its own type
//! and integers arrive as strings, because JSON's number is a double and
//! Firestore's is not.
//!
//! Handing that to serde would mean every configuration struct growing a
//! `#[serde(with = ..)]` per field. So it is decoded here, once, into the shape
//! a configuration file would have had.

use serde_json::{Map, Value};

/// Turns a Firestore `fields` map into plain JSON.
pub(crate) fn to_json(fields: &Value) -> Value {
    let Some(fields) = fields.as_object() else {
        return Value::Object(Map::new());
    };

    let mut out = Map::new();

    for (name, value) in fields {
        out.insert(name.clone(), one(value));
    }

    Value::Object(out)
}

/// One tagged value.
fn one(value: &Value) -> Value {
    let Some(tagged) = value.as_object() else {
        return Value::Null;
    };

    let Some((tag, inner)) = tagged.iter().next() else {
        return Value::Null;
    };

    match tag.as_str() {
        "nullValue" => Value::Null,
        "booleanValue" => inner.clone(),
        "doubleValue" => inner.clone(),
        "stringValue" => inner.clone(),

        // Firestore sends a 64-bit integer as a string, because JSON's number
        // cannot hold one exactly. Parsed back so `port` is a number rather
        // than something a `u16` field refuses.
        "integerValue" => inner
            .as_str()
            .and_then(|text| text.parse::<i64>().ok())
            .map_or_else(|| inner.clone(), Value::from),

        "arrayValue" => inner.get("values").and_then(Value::as_array).map_or_else(
            || Value::Array(Vec::new()),
            |values| Value::Array(values.iter().map(one).collect()),
        ),

        "mapValue" => inner
            .get("fields")
            .map_or_else(|| Value::Object(Map::new()), to_json),

        // A timestamp, a byte string, a reference, a geo point. Each has a
        // faithful string form and no better one — a configuration file would
        // have held a string here too.
        _ => inner.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_integer_comes_back_as_a_number_not_a_string() {
        let fields = serde_json::json!({ "port": { "integerValue": "5432" } });

        assert_eq!(to_json(&fields), serde_json::json!({ "port": 5432 }));
    }

    #[test]
    fn the_ordinary_scalars_pass_through() {
        let fields = serde_json::json!({
            "host": { "stringValue": "localhost" },
            "debug": { "booleanValue": true },
            "ratio": { "doubleValue": 0.5 },
            "absent": { "nullValue": null },
        });

        assert_eq!(
            to_json(&fields),
            serde_json::json!({
                "host": "localhost",
                "debug": true,
                "ratio": 0.5,
                "absent": null,
            })
        );
    }

    #[test]
    fn a_nested_map_becomes_a_nested_table() {
        let fields = serde_json::json!({
            "pool": {
                "mapValue": {
                    "fields": { "max_size": { "integerValue": "10" } }
                }
            }
        });

        assert_eq!(
            to_json(&fields),
            serde_json::json!({ "pool": { "max_size": 10 } })
        );
    }

    #[test]
    fn an_array_keeps_its_order_and_decodes_its_elements() {
        let fields = serde_json::json!({
            "ports": {
                "arrayValue": {
                    "values": [
                        { "integerValue": "1" },
                        { "integerValue": "2" }
                    ]
                }
            }
        });

        assert_eq!(to_json(&fields), serde_json::json!({ "ports": [1, 2] }));
    }

    #[test]
    fn an_empty_array_is_an_empty_array_rather_than_missing() {
        let fields = serde_json::json!({ "ports": { "arrayValue": {} } });

        assert_eq!(to_json(&fields), serde_json::json!({ "ports": [] }));
    }

    #[test]
    fn a_type_with_no_better_form_keeps_its_string() {
        let fields = serde_json::json!({
            "created": { "timestampValue": "2026-01-01T00:00:00Z" }
        });

        assert_eq!(
            to_json(&fields),
            serde_json::json!({ "created": "2026-01-01T00:00:00Z" })
        );
    }

    #[test]
    fn something_that_is_not_a_document_is_an_empty_table() {
        assert_eq!(to_json(&Value::Null), serde_json::json!({}));
    }
}
