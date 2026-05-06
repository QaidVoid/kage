//! JSON Schema derivation helpers for tool inputs.
//!
//! Tools typically declare a strongly-typed `Input` struct with
//! `#[derive(JsonSchema, Deserialize)]` and call [`schema_for`] from
//! [`Tool::schema`](crate::Tool::schema) to surface the schema to the model.

use schemars::JsonSchema;

/// Derive a JSON Schema for a tool input type and return it as a generic
/// `serde_json::Value` ready to embed in [`kage_core::ToolSpec::schema`].
///
/// # Panics
///
/// Panics if the schema does not serialize to JSON, which would indicate a
/// `schemars` bug or an exotic custom impl. Stock derived types never panic.
#[must_use]
pub fn schema_for<T: JsonSchema>() -> serde_json::Value {
    let schema = schemars::schema_for!(T);
    serde_json::to_value(schema).expect("derived schema is always valid JSON")
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemars::JsonSchema;

    #[derive(JsonSchema)]
    #[allow(dead_code)]
    struct ReadInput {
        path: String,
        start_line: Option<u32>,
        end_line: Option<u32>,
    }

    #[test]
    fn derived_schema_has_properties() {
        let s = schema_for::<ReadInput>();
        assert_eq!(s["type"], "object");
        let props = s["properties"]
            .as_object()
            .expect("properties is an object");
        assert!(props.contains_key("path"));
        assert!(props.contains_key("start_line"));
        assert!(props.contains_key("end_line"));
        assert_eq!(props["path"]["type"], "string");
    }

    #[test]
    fn required_fields_are_listed() {
        let s = schema_for::<ReadInput>();
        let required = s["required"]
            .as_array()
            .expect("required is an array")
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>();
        assert!(required.contains(&"path"), "got required {required:?}");
        assert!(
            !required.contains(&"start_line"),
            "Optional fields must not be required"
        );
    }
}
