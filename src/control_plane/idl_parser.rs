//! IDL Parser - Fast zero-copy-inspired parser for Protobuf (.proto) and Thrift (.thrift) definitions
//! into MCP-compatible JSON-RPC schemas. Supports scalar types, repeated/list fields, map<K,V>, and doc comments.

use crate::json::JsonValue;
use crate::models::ProtocolType;
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct FieldDefinition {
    pub name: String,
    pub field_type: String,
    pub is_repeated: bool,
    pub is_required: bool,
    pub is_map: bool,
    pub doc_comment: String,
}

#[derive(Clone, Debug)]
pub struct MethodDefinition {
    pub name: String,
    pub request_type: String,
    pub response_type: String,
    pub fields: Vec<FieldDefinition>,
    pub doc_comment: String,
    pub protocol: ProtocolType,
    pub tags: Vec<String>,
    pub custom_schema: Option<JsonValue>,
    pub is_mutation: bool,
}

#[derive(Clone, Debug)]
pub struct ServiceDefinition {
    pub service_name: String,
    pub package: String,
    pub methods: Vec<MethodDefinition>,
    pub doc_comment: String,
    pub protocol: ProtocolType,
}

pub struct IDLParser;

impl IDLParser {
    pub fn map_primitive_type(idl_type: &str) -> &'static str {
        match idl_type.to_lowercase().as_str() {
            "string" | "bytes" | "binary" => "string",
            "int32" | "int64" | "uint32" | "uint64" | "sint32" | "sint64" | "fixed32"
            | "fixed64" | "sfixed32" | "sfixed64" | "i16" | "i32" | "i64" | "byte" => "integer",
            "bool" | "boolean" => "boolean",
            "float" | "double" => "number",
            _ => "string",
        }
    }

    /// Parses a Protobuf (.proto) string definition into ServiceDefinitions.
    pub fn parse_protobuf(content: &str) -> Vec<ServiceDefinition> {
        let mut services = Vec::new();
        let mut package = "uber.service".to_string();

        for line in content.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("package ") {
                if let Some(pkg) = rest.strip_suffix(';') {
                    package = pkg.trim().to_string();
                }
            }
        }

        // 1. Extract `message Name { ... }` blocks
        let mut messages: HashMap<String, Vec<FieldDefinition>> = HashMap::new();
        for (_doc, name, body) in extract_named_blocks(content, "message") {
            let mut fields = Vec::new();
            for raw_line in body.lines() {
                let line = raw_line.trim();
                if line.is_empty() || line.starts_with("//") {
                    continue;
                }
                let (code_part, inline_comment) = match line.split_once("//") {
                    Some((c, cmt)) => (c.trim(), cmt.trim().to_string()),
                    None => (line, String::new()),
                };
                // Strip `= <tag>;`
                let before_eq = match code_part.split_once('=') {
                    Some((left, _)) => left.trim(),
                    None => continue,
                };
                if before_eq.starts_with("map<") {
                    if let Some(close_idx) = before_eq.find('>') {
                        let map_type = before_eq[..=close_idx].trim().to_string();
                        let fname = before_eq[close_idx + 1..].trim().to_string();
                        if !fname.is_empty() {
                            fields.push(FieldDefinition {
                                name: fname,
                                field_type: map_type,
                                is_repeated: false,
                                is_required: false,
                                is_map: true,
                                doc_comment: inline_comment,
                            });
                        }
                    }
                    continue;
                }

                let tokens: Vec<&str> = before_eq.split_whitespace().collect();
                if tokens.len() >= 2 {
                    let (is_rep, is_req, ftype, fname) = match tokens[0] {
                        "repeated" if tokens.len() >= 3 => (true, false, tokens[1], tokens[2]),
                        "required" if tokens.len() >= 3 => (false, true, tokens[1], tokens[2]),
                        "optional" if tokens.len() >= 3 => (false, false, tokens[1], tokens[2]),
                        _ => (false, false, tokens[0], tokens[1]),
                    };
                    fields.push(FieldDefinition {
                        name: fname.to_string(),
                        field_type: ftype.to_string(),
                        is_repeated: is_rep,
                        is_required: is_req,
                        is_map: false,
                        doc_comment: inline_comment,
                    });
                }
            }
            messages.insert(name, fields);
        }

        // 2. Extract `service Name { ... }` blocks
        for (svc_doc, svc_name, svc_body) in extract_named_blocks(content, "service") {
            let mut methods = Vec::new();
            let mut pending_comment = String::new();

            for raw_line in svc_body.lines() {
                let line = raw_line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Some(cmt) = line.strip_prefix("//") {
                    pending_comment = cmt.trim().to_string();
                    continue;
                }
                if let Some(rest) = line.strip_prefix("rpc ") {
                    // Format: MethodName (ReqType) returns (RespType);
                    if let Some((method_part, after_open)) = rest.split_once('(') {
                        let method_name = method_part.trim().to_string();
                        if let Some((req_part, after_req)) = after_open.split_once(')') {
                            let req_type = req_part
                                .trim()
                                .trim_start_matches("stream ")
                                .trim()
                                .to_string();
                            if let Some((_, resp_open)) = after_req.split_once('(') {
                                if let Some((resp_part, _)) = resp_open.split_once(')') {
                                    let resp_type = resp_part
                                        .trim()
                                        .trim_start_matches("stream ")
                                        .trim()
                                        .to_string();
                                    let method_fields =
                                        messages.get(&req_type).cloned().unwrap_or_default();
                                    let is_mutation = is_mutation_name(&method_name);
                                    methods.push(MethodDefinition {
                                        name: method_name,
                                        request_type: req_type,
                                        response_type: resp_type,
                                        fields: method_fields,
                                        doc_comment: std::mem::take(&mut pending_comment),
                                        protocol: ProtocolType::Grpc,
                                        tags: Vec::new(),
                                        custom_schema: None,
                                        is_mutation,
                                    });
                                }
                            }
                        }
                    }
                }
            }

            services.push(ServiceDefinition {
                service_name: svc_name,
                package: package.clone(),
                methods,
                doc_comment: svc_doc,
                protocol: ProtocolType::Grpc,
            });
        }

        services
    }

    /// Parses a Thrift (.thrift) string definition into ServiceDefinitions (TChannel protocol).
    pub fn parse_thrift(content: &str) -> Vec<ServiceDefinition> {
        let mut services = Vec::new();
        let mut package = "uber.tchannel".to_string();

        for line in content.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("namespace ") {
                let parts: Vec<&str> = rest.split_whitespace().collect();
                if parts.len() >= 2 {
                    package = parts[1].to_string();
                }
            }
        }

        for (svc_doc, svc_name, svc_body) in extract_named_blocks(content, "service") {
            let mut methods = Vec::new();
            let mut pending_comment = String::new();

            for raw_line in svc_body.lines() {
                let line = raw_line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Some(cmt) = line.strip_prefix("//") {
                    pending_comment = cmt.trim().to_string();
                    continue;
                }
                // Format: ReturnType methodName(1: string arg1, 2: bool arg2)
                if let Some((sig_before_paren, after_open)) = line.split_once('(') {
                    if let Some((params_str, _)) = after_open.split_once(')') {
                        let sig_tokens: Vec<&str> = sig_before_paren.split_whitespace().collect();
                        if sig_tokens.len() >= 2 {
                            let resp_type = sig_tokens[sig_tokens.len() - 2].to_string();
                            let m_name = sig_tokens[sig_tokens.len() - 1].to_string();

                            let mut fields = Vec::new();
                            for param in params_str.split(',') {
                                let p = param.trim().trim_end_matches(';');
                                if p.is_empty() {
                                    continue;
                                }
                                // Strip `1:`
                                let after_colon = match p.split_once(':') {
                                    Some((_, rhs)) => rhs.trim(),
                                    None => p,
                                };
                                let p_tokens: Vec<&str> = after_colon.split_whitespace().collect();
                                if p_tokens.len() >= 2 {
                                    let (is_req, type_tok, name_tok) = match p_tokens[0] {
                                        "required" if p_tokens.len() >= 3 => {
                                            (true, p_tokens[1], p_tokens[2])
                                        }
                                        "optional" if p_tokens.len() >= 3 => {
                                            (false, p_tokens[1], p_tokens[2])
                                        }
                                        _ => (false, p_tokens[0], p_tokens[1]),
                                    };
                                    let is_rep = type_tok.starts_with("list<")
                                        || type_tok.starts_with("set<");
                                    let inner_type = if is_rep {
                                        type_tok
                                            .trim_start_matches("list<")
                                            .trim_start_matches("set<")
                                            .trim_end_matches('>')
                                            .to_string()
                                    } else {
                                        type_tok.to_string()
                                    };
                                    fields.push(FieldDefinition {
                                        name: name_tok.to_string(),
                                        field_type: inner_type,
                                        is_repeated: is_rep,
                                        is_required: is_req,
                                        is_map: type_tok.starts_with("map<"),
                                        doc_comment: String::new(),
                                    });
                                }
                            }

                            let is_mutation = is_mutation_name(&m_name);
                            methods.push(MethodDefinition {
                                name: m_name.clone(),
                                request_type: format!("{}Request", m_name),
                                response_type: resp_type,
                                fields,
                                doc_comment: std::mem::take(&mut pending_comment),
                                protocol: ProtocolType::TChannel,
                                tags: Vec::new(),
                                custom_schema: None,
                                is_mutation,
                            });
                        }
                    }
                }
            }

            services.push(ServiceDefinition {
                service_name: svc_name,
                package: package.clone(),
                methods,
                doc_comment: svc_doc,
                protocol: ProtocolType::TChannel,
            });
        }

        services
    }

    /// Parses an OpenAPI 3.0, 3.1, or Swagger 2.0 definition into ServiceDefinitions.
    pub fn parse_openapi(content: &str) -> Vec<ServiceDefinition> {
        let root = match JsonValue::parse(content) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };

        let title = root
            .get("info")
            .and_then(|i| i.get("title"))
            .and_then(|t| t.as_str())
            .unwrap_or("OpenAPIService");
        let svc_name: String = title
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
            .collect();
        let svc_name = if svc_name.is_empty() {
            "OpenAPIService".to_string()
        } else {
            svc_name
        };

        let svc_doc = root
            .get("info")
            .and_then(|i| i.get("description"))
            .and_then(|d| d.as_str())
            .unwrap_or("")
            .to_string();

        let mut methods = Vec::new();

        if let Some(paths) = root.get("paths").and_then(|p| p.as_object()) {
            for (path_str, path_item) in paths {
                if let Some(ops) = path_item.as_object() {
                    for (http_method, op_val) in ops {
                        let http_method_upper = http_method.to_uppercase();
                        if !matches!(
                            http_method_upper.as_str(),
                            "GET" | "POST" | "PUT" | "DELETE" | "PATCH"
                        ) {
                            continue;
                        }

                        let op_id = op_val
                            .get("operationId")
                            .and_then(|v| v.as_str())
                            .map(String::from)
                            .unwrap_or_else(|| {
                                let clean_path: String = path_str
                                    .chars()
                                    .map(|c| if c.is_alphanumeric() { c } else { '_' })
                                    .collect();
                                format!(
                                    "{}_{}",
                                    http_method.to_lowercase(),
                                    clean_path.trim_matches('_')
                                )
                            });

                        let summary = op_val
                            .get("summary")
                            .or_else(|| op_val.get("description"))
                            .and_then(|s| s.as_str())
                            .unwrap_or("")
                            .to_string();

                        let mut tags = Vec::new();
                        if let Some(tag_arr) = op_val.get("tags").and_then(|t| t.as_array()) {
                            for tag in tag_arr {
                                if let Some(ts) = tag.as_str() {
                                    tags.push(ts.to_string());
                                }
                            }
                        }

                        let mut fields = Vec::new();
                        let mut properties = JsonValue::object();
                        let mut required_keys = Vec::new();

                        if let Some(params) = op_val.get("parameters").and_then(|p| p.as_array()) {
                            for param in params {
                                if let Some(p_name) = param.get("name").and_then(|n| n.as_str()) {
                                    let p_desc = param
                                        .get("description")
                                        .and_then(|d| d.as_str())
                                        .unwrap_or("");
                                    let p_type = param
                                        .get("schema")
                                        .and_then(|s| s.get("type"))
                                        .and_then(|t| t.as_str())
                                        .unwrap_or("string");
                                    let p_req = param
                                        .get("required")
                                        .and_then(|r| r.as_bool())
                                        .unwrap_or(false);

                                    properties.insert(
                                        p_name.to_string(),
                                        json!({
                                            "type": p_type,
                                            "description": p_desc
                                        }),
                                    );
                                    if p_req {
                                        required_keys.push(JsonValue::from(p_name));
                                    }
                                    fields.push(FieldDefinition {
                                        name: p_name.to_string(),
                                        field_type: p_type.to_string(),
                                        is_repeated: false,
                                        is_required: p_req,
                                        is_map: false,
                                        doc_comment: p_desc.to_string(),
                                    });
                                }
                            }
                        }

                        if let Some(rb_schema) = op_val
                            .get("requestBody")
                            .and_then(|rb| rb.get("content"))
                            .and_then(|c| c.get("application/json"))
                            .and_then(|aj| aj.get("schema"))
                        {
                            if let Some(rb_props) =
                                rb_schema.get("properties").and_then(|p| p.as_object())
                            {
                                for (k, v) in rb_props {
                                    properties.insert(k.clone(), v.clone());
                                }
                            }
                            if let Some(rb_req) =
                                rb_schema.get("required").and_then(|r| r.as_array())
                            {
                                for r in rb_req {
                                    required_keys.push(r.clone());
                                }
                            }
                        }

                        let custom_schema = json!({
                            "type": "object",
                            "properties": properties,
                            "required": JsonValue::Array(required_keys)
                        });

                        let is_mutation = matches!(
                            http_method_upper.as_str(),
                            "POST" | "PUT" | "DELETE" | "PATCH"
                        );

                        methods.push(MethodDefinition {
                            name: op_id,
                            request_type: format!("{}/{}", http_method_upper, path_str),
                            response_type: "200".to_string(),
                            fields,
                            doc_comment: summary,
                            protocol: ProtocolType::Http,
                            tags,
                            custom_schema: Some(custom_schema),
                            is_mutation,
                        });
                    }
                }
            }
        }

        vec![ServiceDefinition {
            service_name: svc_name,
            package: "openapi.rest".to_string(),
            methods,
            doc_comment: svc_doc,
            protocol: ProtocolType::Http,
        }]
    }

    /// Translates method parameter fields into an MCP JSON Schema.
    pub fn translate_to_json_schema(method: &MethodDefinition) -> JsonValue {
        if let Some(cs) = &method.custom_schema {
            return cs.clone();
        }
        let mut properties = JsonValue::object();
        let mut required: Vec<JsonValue> = Vec::new();

        for field_def in &method.fields {
            let json_type = Self::map_primitive_type(&field_def.field_type);
            let desc = if field_def.doc_comment.is_empty() {
                format!("Field {} ({})", field_def.name, field_def.field_type)
            } else {
                field_def.doc_comment.clone()
            };

            let prop = if field_def.is_map {
                json!({
                    "type": "object",
                    "description": desc,
                })
            } else if field_def.is_repeated {
                json!({
                    "type": "array",
                    "description": desc,
                    "items": {
                        "type": json_type,
                    }
                })
            } else {
                json!({
                    "type": json_type,
                    "description": desc,
                })
            };

            properties.insert(field_def.name.clone(), prop);
            if field_def.is_required {
                required.push(JsonValue::from(field_def.name.as_str()));
            }
        }

        let mut schema = json!({
            "type": "object",
            "properties": properties,
        });
        if !required.is_empty() {
            schema.insert("required", JsonValue::Array(required));
        }
        schema
    }
}

fn extract_named_blocks(content: &str, keyword: &str) -> Vec<(String, String, String)> {
    let mut results = Vec::new();
    let lines: Vec<&str> = content.lines().collect();
    let mut idx = 0;
    let mut last_comment = String::new();

    while idx < lines.len() {
        let trimmed = lines[idx].trim();
        if let Some(cmt) = trimmed.strip_prefix("//") {
            last_comment = cmt.trim().to_string();
            idx += 1;
            continue;
        }
        let prefix = format!("{} ", keyword);
        if let Some(after_kw) = trimmed.strip_prefix(&prefix) {
            // Find opening brace
            let mut combined = after_kw.to_string();
            while !combined.contains('{') && idx + 1 < lines.len() {
                idx += 1;
                combined.push(' ');
                combined.push_str(lines[idx].trim());
            }
            if let Some((name_part, after_brace)) = combined.split_once('{') {
                let block_name = name_part
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_string();
                let mut body = String::new();
                let mut depth = 1usize;

                for ch in after_brace.chars() {
                    if ch == '{' {
                        depth += 1;
                        body.push(ch);
                    } else if ch == '}' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                        body.push(ch);
                    } else {
                        body.push(ch);
                    }
                }

                while depth > 0 && idx + 1 < lines.len() {
                    idx += 1;
                    body.push('\n');
                    for ch in lines[idx].chars() {
                        if ch == '{' {
                            depth += 1;
                            body.push(ch);
                        } else if ch == '}' {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                            body.push(ch);
                        } else {
                            body.push(ch);
                        }
                    }
                }

                results.push((std::mem::take(&mut last_comment), block_name, body));
            }
        } else if !trimmed.is_empty() {
            last_comment.clear();
        }
        idx += 1;
    }

    results
}

fn is_mutation_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.starts_with("set")
        || lower.starts_with("create")
        || lower.starts_with("update")
        || lower.starts_with("delete")
        || lower.starts_with("post")
        || lower.starts_with("cancel")
        || lower.starts_with("dispatch")
        || lower.starts_with("deliver")
        || lower.starts_with("charge")
        || lower.starts_with("refund")
}
