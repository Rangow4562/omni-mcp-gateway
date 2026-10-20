//! JSON-RPC 2.0 and Model Context Protocol (MCP) data models, batch support, and error primitives.

use crate::json::JsonValue;

pub const JSONRPC_VERSION: &str = "2.0";

// Standard JSON-RPC 2.0 Error Codes
pub const PARSE_ERROR: i32 = -32700;
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;
pub const INTERNAL_ERROR: i32 = -32603;

// MCP & Enterprise Gateway Specific Error Codes
pub const UNAUTHORIZED: i32 = -32001;
pub const TOOL_DISABLED: i32 = -32002;
pub const DOWNSTREAM_ERROR: i32 = -32003;
pub const RATE_LIMITED: i32 = -32029;
pub const CIRCUIT_OPEN: i32 = -32030;

#[derive(Clone, Debug)]
pub struct RPCError {
    pub code: i32,
    pub message: String,
    pub data: Option<JsonValue>,
}

impl RPCError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(code: i32, message: impl Into<String>, data: JsonValue) -> Self {
        Self {
            code,
            message: message.into(),
            data: Some(data),
        }
    }

    pub fn to_json(&self) -> JsonValue {
        let mut obj = json!({
            "code": self.code,
            "message": self.message.clone(),
        });
        if let Some(d) = &self.data {
            obj.insert("data", d.clone());
        }
        obj
    }
}

#[derive(Clone, Debug)]
pub struct RPCRequest {
    pub jsonrpc: String,
    pub id: JsonValue,
    pub method: String,
    pub params: JsonValue,
}

impl RPCRequest {
    pub fn new(id: impl Into<JsonValue>, method: impl Into<String>, params: JsonValue) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: id.into(),
            method: method.into(),
            params,
        }
    }

    pub fn from_json(data: &JsonValue) -> Result<Self, String> {
        if !data.is_object() {
            return Err("Request must be a JSON object".to_string());
        }
        let method = data
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        let params = data
            .get("params")
            .cloned()
            .unwrap_or_else(JsonValue::object);
        let id = data.get("id").cloned().unwrap_or(JsonValue::Null);
        let jsonrpc = data
            .get("jsonrpc")
            .and_then(|v| v.as_str())
            .unwrap_or(JSONRPC_VERSION)
            .to_string();

        Ok(Self {
            jsonrpc,
            id,
            method,
            params,
        })
    }
}

#[derive(Clone, Debug)]
pub enum RPCBatchOrSingle {
    Single(RPCRequest),
    Batch(Vec<RPCRequest>),
}

impl RPCBatchOrSingle {
    pub fn from_json(data: &JsonValue) -> Result<Self, String> {
        if let Some(arr) = data.as_array() {
            if arr.is_empty() {
                return Err("Batch JSON-RPC request array cannot be empty".to_string());
            }
            let mut reqs = Vec::with_capacity(arr.len());
            for item in arr {
                reqs.push(RPCRequest::from_json(item)?);
            }
            Ok(RPCBatchOrSingle::Batch(reqs))
        } else {
            Ok(RPCBatchOrSingle::Single(RPCRequest::from_json(data)?))
        }
    }
}

#[derive(Clone, Debug)]
pub struct RPCResponse {
    pub jsonrpc: String,
    pub id: JsonValue,
    pub result: Option<JsonValue>,
    pub error: Option<RPCError>,
}

impl RPCResponse {
    pub fn ok(id: JsonValue, result: JsonValue) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: JsonValue, error: RPCError) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: None,
            error: Some(error),
        }
    }

    pub fn to_json(&self) -> JsonValue {
        let mut out = json!({
            "jsonrpc": self.jsonrpc.clone(),
            "id": self.id.clone(),
        });
        if let Some(err) = &self.error {
            out.insert("error", err.to_json());
        } else {
            out.insert(
                "result",
                self.result.clone().unwrap_or_else(JsonValue::object),
            );
        }
        out
    }
}
