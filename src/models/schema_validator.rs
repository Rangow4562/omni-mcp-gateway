//! Schema Preflight Validator (-32035 SCHEMA_PREFLIGHT_FAILED).
//! Performs static AST/JSON argument type-checking against registered `ToolRecord.input_schema`
//! before execution. Collects all violations in a single pass into `RPCError.data` for single-turn self-correction.

use crate::json::JsonValue;

/// Represents a single schema validation violation.
#[derive(Clone, Debug, PartialEq)]
pub struct SchemaViolation {
    pub path: String,
    pub expected: String,
    pub actual: String,
    pub message: String,
}

impl SchemaViolation {
    pub fn to_json(&self) -> JsonValue {
        json!({
            "path": self.path.clone(),
            "expected": self.expected.clone(),
            "actual": self.actual.clone(),
            "message": self.message.clone(),
        })
    }
}

/// Validates arguments against a JSON Schema.
/// Collects all violations across all fields in a single pass.
pub fn validate_tool_schema(
    schema: &JsonValue,
    args: &JsonValue,
) -> Result<(), Vec<SchemaViolation>> {
    let mut violations = Vec::new();
    validate_value(schema, args, "", &mut violations);
    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}

fn json_type_name(v: &JsonValue) -> &'static str {
    match v {
        JsonValue::Null => "null",
        JsonValue::Bool(_) => "boolean",
        JsonValue::Int(_) => "integer",
        JsonValue::Float(_) => "number",
        JsonValue::Str(_) => "string",
        JsonValue::Array(_) => "array",
        JsonValue::Object(_) => "object",
    }
}

fn validate_value(
    schema: &JsonValue,
    val: &JsonValue,
    path: &str,
    violations: &mut Vec<SchemaViolation>,
) {
    let expected_type = schema.get("type").and_then(|t| t.as_str());

    if let Some(exp_type) = expected_type {
        let type_match = match exp_type {
            "object" => val.is_object(),
            "array" => val.is_array(),
            "string" => val.is_string(),
            "number" => val.as_f64().is_some(),
            "integer" => matches!(val, JsonValue::Int(_)),
            "boolean" => val.as_bool().is_some(),
            "null" => val.is_null(),
            _ => true,
        };

        if !type_match {
            let p = if path.is_empty() { "root" } else { path };
            violations.push(SchemaViolation {
                path: p.to_string(),
                expected: exp_type.to_string(),
                actual: json_type_name(val).to_string(),
                message: format!(
                    "Expected type '{}' at '{}', found '{}'",
                    exp_type,
                    p,
                    json_type_name(val)
                ),
            });
            return;
        }
    }

    if let Some(entries) = val.as_object() {
        // Validate required fields
        if let Some(req_arr) = schema.get("required").and_then(|r| r.as_array()) {
            for req in req_arr {
                if let Some(req_name) = req.as_str() {
                    let field_path = if path.is_empty() {
                        req_name.to_string()
                    } else {
                        format!("{}.{}", path, req_name)
                    };
                    let present = entries.iter().any(|(k, v)| k == req_name && !v.is_null());
                    if !present {
                        violations.push(SchemaViolation {
                            path: field_path.clone(),
                            expected: "present".to_string(),
                            actual: "missing".to_string(),
                            message: format!("Required field '{}' is missing", field_path),
                        });
                    }
                }
            }
        }

        // Validate defined properties
        if let Some(props) = schema.get("properties").and_then(|p| p.as_object()) {
            for (prop_name, prop_schema) in props {
                if let Some(prop_val) = val.get(prop_name) {
                    let prop_path = if path.is_empty() {
                        prop_name.clone()
                    } else {
                        format!("{}.{}", path, prop_name)
                    };
                    validate_value(prop_schema, prop_val, &prop_path, violations);
                }
            }
        }
    } else if let Some(arr) = val.as_array() {
        if let Some(item_schema) = schema.get("items") {
            for (idx, item) in arr.iter().enumerate() {
                let item_path = format!("{}[{}]", path, idx);
                validate_value(item_schema, item, &item_path, violations);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_schema_preflight_collects_all_violations_in_single_pass() {
        let schema = json!({
            "type": "object",
            "required": ["user_id", "amount", "active"],
            "properties": {
                "user_id": {"type": "string"},
                "amount": {"type": "number"},
                "active": {"type": "boolean"},
                "tags": {
                    "type": "array",
                    "items": {"type": "string"}
                }
            }
        });

        // Arguments with multiple violations:
        // 1. missing `user_id`
        // 2. `amount` is string instead of number
        // 3. missing `active`
        // 4. `tags[1]` is integer instead of string
        let invalid_args = json!({
            "amount": "not_a_number",
            "tags": ["valid_tag", 12345]
        });

        let res = validate_tool_schema(&schema, &invalid_args);
        assert!(res.is_err());
        let violations = res.unwrap_err();
        assert_eq!(
            violations.len(),
            4,
            "Must collect all 4 violations: {:?}",
            violations
        );

        let paths: Vec<&str> = violations.iter().map(|v| v.path.as_str()).collect();
        assert!(paths.contains(&"user_id"));
        assert!(paths.contains(&"amount"));
        assert!(paths.contains(&"active"));
        assert!(paths.contains(&"tags[1]"));
    }

    #[test]
    fn test_schema_preflight_passes_valid_arguments() {
        let schema = json!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string"},
                "count": {"type": "integer"}
            }
        });

        let valid_args = json!({
            "id": "item_123",
            "count": 42
        });

        assert!(validate_tool_schema(&schema, &valid_args).is_ok());
    }
}
