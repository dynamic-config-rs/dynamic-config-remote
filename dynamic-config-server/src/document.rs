//! The served document: one resolved section, as data rather than a struct.
//!
//! A configuration server does not know its callers' types — that is the
//! whole point of it — so the served shape is schemaless. Everything else in
//! this workspace resolves a section *into* a struct; here the section is
//! resolved into JSON and handed over, which is what makes one server able
//! to serve a Rust service, a Python service and a shell script.
//!
//! JSON rather than [`dynamic_config::Value`] because the wire format is
//! JSON and the conversion has to happen exactly once. `Value` is the
//! library's owned mirror for boundaries that are *not* serde; this boundary
//! is serde, and routing through a second tree would only add a place for
//! the two renderings to disagree.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A resolved configuration section.
///
/// # `Debug` prints shape, never values
///
/// Hand-written for the reason `Snapshot` and `Value` are: this type holds
/// the resolved configuration, passwords included, and `{:?}` in a log line
/// is exactly how a resolved secret escapes. The values leave this process
/// through one door — the document endpoint — and a `Debug` that rendered
/// them would quietly open a second.
#[derive(Clone, Deserialize, Serialize)]
#[serde(transparent)]
pub struct Document(serde_json::Value);

impl Document {
    /// The document as JSON, for the one endpoint that serves values.
    #[must_use]
    pub fn as_json(&self) -> &serde_json::Value {
        &self.0
    }

    /// The dotted path of every leaf, in order.
    ///
    /// The same walk [`Snapshot::leaf_paths`](dynamic_config::Snapshot::leaf_paths)
    /// performs, and for the same purpose: a caller that wants to know
    /// *which keys exist* without being handed what is in them. An array is
    /// a leaf — its elements are values, not configuration keys — and so is
    /// an empty table, which would otherwise vanish from the listing
    /// entirely.
    #[must_use]
    pub fn leaf_paths(&self) -> Vec<String> {
        let mut paths = Vec::new();

        // The root is walked here rather than in `collect` so that the
        // recursion never has to ask "am I at the top?" — which is what a
        // configuration key that is the empty string would make it get
        // wrong, and TOML permits one.
        if let serde_json::Value::Object(table) = &self.0 {
            let mut prefix = String::new();

            for (key, value) in table {
                prefix.clear();
                prefix.push_str(key);

                collect(value, &mut prefix, &mut paths);
            }
        }

        paths
    }
}

fn collect(value: &serde_json::Value, prefix: &mut String, paths: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(table) if !table.is_empty() => {
            for (key, nested) in table {
                let restore = prefix.len();

                prefix.push('.');
                prefix.push_str(key);

                collect(nested, prefix, paths);

                prefix.truncate(restore);
            }
        }
        _ => paths.push(prefix.clone()),
    }
}

impl fmt::Debug for Document {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Document")
            .field("leaves", &self.leaf_paths().len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(json: serde_json::Value) -> Document {
        serde_json::from_value(json).expect("any JSON is a document")
    }

    #[test]
    fn leaf_paths_reach_into_nested_tables_and_stop_at_arrays() {
        let document = document(serde_json::json!({
            "host": "db",
            "pool": { "max": 8, "min": 1 },
            "tags": ["a", "b"],
            "empty": {},
        }));

        assert_eq!(
            document.leaf_paths(),
            ["empty", "host", "pool.max", "pool.min", "tags"]
        );
    }

    /// TOML and JSON both permit `"" = 1`, and the walk must not lose it or
    /// render it as the path of its parent.
    #[test]
    fn a_key_that_is_the_empty_string_still_has_a_path() {
        let document = document(serde_json::json!({ "": 1, "pool": { "": 2 } }));

        assert_eq!(document.leaf_paths(), ["", "pool."]);
    }

    #[test]
    fn a_document_that_is_not_a_table_has_no_paths() {
        assert!(document(serde_json::json!(7)).leaf_paths().is_empty());
        assert!(document(serde_json::json!({})).leaf_paths().is_empty());
    }

    /// The rule the rest of the workspace keeps, kept here: a `{:?}` of a
    /// resolved configuration must not be the leak everything else prevents.
    #[test]
    fn debug_prints_shape_and_never_a_value() {
        let document = document(serde_json::json!({ "password": "hunter2" }));

        let rendered = format!("{document:?}");

        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(!rendered.contains("password"), "{rendered}");
    }
}
