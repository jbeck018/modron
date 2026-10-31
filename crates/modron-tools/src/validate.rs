//! Argument validation (LEDGER T3) — JSON-schema checks applied before any
//! tool body runs. Bad arguments produce a tool-error outcome with a helpful
//! message, never a panic.

use modron_core::ToolSpec;

/// Validates tool-call arguments against a tool's JSON-schema `parameters`.
///
/// Checks (top level only — nested schemas are not enforced):
/// - `args` is a JSON object;
/// - every key in the schema's `required` list is present;
/// - no unknown top-level keys (enforced when the schema declares
///   `properties`; a schema without `properties` accepts any keys);
/// - each present property's value matches its declared `type`
///   (`string`, `number`, `integer`, `boolean`, `array`, `object`, `null`),
///   where `integer` accepts integral floats like `2000.0`.
///
/// Returns `Err(message)` describing the first problem found.
pub fn validate_args(spec: &ToolSpec, args: &serde_json::Value) -> Result<(), String> {
    let obj = args.as_object().ok_or_else(|| {
        format!(
            "arguments must be a JSON object, got {}",
            json_type_name(args)
        )
    })?;

    let schema = spec.parameters.as_object();

    // Required keys.
    if let Some(required) = schema
        .and_then(|s| s.get("required"))
        .and_then(|r| r.as_array())
    {
        for key in required {
            match key.as_str() {
                Some(k) if !obj.contains_key(k) => {
                    return Err(format!("missing required argument `{k}`"));
                }
                _ => {}
            }
        }
    }

    // Unknown keys — only when the schema declares its properties.
    if let Some(properties) = schema
        .and_then(|s| s.get("properties"))
        .and_then(|p| p.as_object())
    {
        for key in obj.keys() {
            if !properties.contains_key(key) {
                let known: Vec<&String> = properties.keys().collect();
                return Err(format!(
                    "unknown argument `{key}` (expected one of: {})",
                    known
                        .iter()
                        .map(|k| k.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
        // Type checks for present, typed properties.
        for (key, value) in obj {
            let declared = properties
                .get(key)
                .and_then(|p| p.get("type"))
                .and_then(|t| t.as_str());
            if let Some(expected) = declared {
                if !type_matches(expected, value) {
                    return Err(format!(
                        "argument `{key}` must be of type {expected}, got {}",
                        json_type_name(value)
                    ));
                }
            }
        }
    }

    Ok(())
}

fn type_matches(expected: &str, value: &serde_json::Value) -> bool {
    match expected {
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => match value {
            serde_json::Value::Number(n) => {
                n.is_i64() || n.is_u64() || n.as_f64().is_some_and(|f| f.fract() == 0.0)
            }
            _ => false,
        },
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "null" => value.is_null(),
        // Unrecognized/absent type keywords are not enforced.
        _ => true,
    }
}

fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(params: serde_json::Value) -> ToolSpec {
        ToolSpec {
            name: "t".into(),
            description: String::new(),
            parameters: params,
        }
    }

    fn schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "limit": { "type": "integer" },
                "overwrite": { "type": "boolean" }
            },
            "required": ["path"]
        })
    }

    #[test]
    fn valid_args_pass() {
        let s = spec(schema());
        assert!(validate_args(&s, &json!({"path": "a.rs"})).is_ok());
        assert!(validate_args(&s, &json!({"path": "a.rs", "limit": 10})).is_ok());
        // Integral float is accepted for `integer`.
        assert!(validate_args(&s, &json!({"path": "a.rs", "limit": 10.0})).is_ok());
    }

    #[test]
    fn non_object_args_rejected() {
        let s = spec(schema());
        let err = validate_args(&s, &json!("not an object")).unwrap_err();
        assert!(err.contains("must be a JSON object"), "{err}");
    }

    #[test]
    fn missing_required_names_the_key() {
        let s = spec(schema());
        let err = validate_args(&s, &json!({"limit": 5})).unwrap_err();
        assert!(err.contains("missing required argument `path`"), "{err}");
    }

    #[test]
    fn unknown_key_rejected() {
        let s = spec(schema());
        let err = validate_args(&s, &json!({"path": "a", "bogus": 1})).unwrap_err();
        assert!(err.contains("unknown argument `bogus`"), "{err}");
    }

    #[test]
    fn wrong_type_rejected() {
        let s = spec(schema());
        let err = validate_args(&s, &json!({"path": 42})).unwrap_err();
        assert!(
            err.contains("`path` must be of type string, got number"),
            "{err}"
        );
        let err = validate_args(&s, &json!({"path": "a", "overwrite": "yes"})).unwrap_err();
        assert!(err.contains("`overwrite` must be of type boolean"), "{err}");
    }

    #[test]
    fn schema_without_properties_accepts_any_keys() {
        let s = spec(json!({"type": "object"}));
        assert!(validate_args(&s, &json!({})).is_ok());
        assert!(validate_args(&s, &json!({"anything": true})).is_ok());
    }
}
