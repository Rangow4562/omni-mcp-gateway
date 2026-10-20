//! Response Projection Engine - GraphQL-like field-level projection pattern for MCP tool responses.
//! Trims runtime JSON responses by retaining only explicitly projected nested fields (`driver.vehicle.make`, `trips[].id`).

use crate::json::JsonValue;

pub struct ResponseProjectionEngine;

impl ResponseProjectionEngine {
    pub fn apply_projection(
        response_data: &JsonValue,
        projection_paths: Option<&[String]>,
    ) -> JsonValue {
        let paths = match projection_paths {
            Some(p) if !p.is_empty() => p,
            _ => return response_data.clone(),
        };
        if !response_data.is_object() {
            return response_data.clone();
        }

        let mut result = JsonValue::object();
        for path in paths {
            let parts: Vec<&str> = path
                .trim()
                .split('.')
                .filter(|s| !s.is_empty())
                .collect();
            if !parts.is_empty() {
                Self::extract_nested_path(response_data, &parts, &mut result);
            }
        }
        result
    }

    fn extract_nested_path(source: &JsonValue, parts: &[&str], target: &mut JsonValue) {
        if parts.is_empty() || !source.is_object() {
            return;
        }

        let current_key = parts[0];
        if current_key == "*" {
            if let Some(entries) = source.as_object() {
                for (k, v) in entries {
                    if parts.len() == 1 {
                        target.insert(k.clone(), v.clone());
                    } else {
                        if !target.get(k).map_or(false, |x| x.is_object()) {
                            target.insert(k.clone(), JsonValue::object());
                        }
                        if let Some(sub_target) = target.get_mut(k) {
                            Self::extract_nested_path(v, &parts[1..], sub_target);
                        }
                    }
                }
            }
            return;
        }

        let is_array = current_key.ends_with("[]");
        let clean_key = if is_array {
            &current_key[..current_key.len() - 2]
        } else {
            current_key
        };

        let source_val = match source.get(clean_key) {
            Some(v) => v,
            None => return,
        };

        if parts.len() == 1 {
            target.insert(clean_key.to_string(), source_val.clone());
            return;
        }

        let remaining_parts = &parts[1..];
        if is_array {
            let source_arr = match source_val.as_array() {
                Some(a) => a,
                None => return,
            };
            if !target.get(clean_key).map_or(false, |x| x.is_array()) {
                target.insert(clean_key.to_string(), JsonValue::array());
            }
            if let Some(target_arr) = target.get_mut(clean_key).and_then(|x| x.as_array_mut()) {
                for (idx, item) in source_arr.iter().enumerate() {
                    if idx >= target_arr.len() {
                        target_arr.push(JsonValue::object());
                    }
                    if item.is_object() {
                        Self::extract_nested_path(item, remaining_parts, &mut target_arr[idx]);
                    } else {
                        target_arr[idx] = item.clone();
                    }
                }
            }
        } else {
            if !target.get(clean_key).map_or(false, |x| x.is_object()) {
                target.insert(clean_key.to_string(), JsonValue::object());
            }
            if let Some(sub_target) = target.get_mut(clean_key) {
                Self::extract_nested_path(source_val, remaining_parts, sub_target);
            }
        }
    }
}

