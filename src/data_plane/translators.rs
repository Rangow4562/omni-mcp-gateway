//! Protocol Translators - Converts MCP JSON-RPC payloads into in-memory HTTP, gRPC (Protobuf length-prefixed), and TChannel (Thrift) wire envelope frames.
//! When the `transport-http` feature is enabled and a tool provides a live `http://` downstream endpoint,
//! dispatches live network I/O via `HttpDownstreamClient`.

use crate::control_plane::third_party::ThirdPartyAuthExchange;
use crate::data_plane::cache::InMemoryConfigCache;
use crate::data_plane::downstream_http::HttpDownstreamClient;
use crate::data_plane::muttley::MuttleyMeshClient;
use crate::json::JsonValue;
use crate::models::{ProtocolType, ToolRecord};
use crate::upstream::UpstreamManager;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone, Default)]
pub struct ProtocolTranslator {
    pub mesh_client: MuttleyMeshClient,
    pub http_client: HttpDownstreamClient,
    pub auth_exchange: ThirdPartyAuthExchange,
    pub upstream_manager: Option<Arc<UpstreamManager>>,
    pub config_cache: Option<InMemoryConfigCache>,
}

impl ProtocolTranslator {
    pub fn new(
        mesh_client: Option<MuttleyMeshClient>,
        auth_exchange: Option<ThirdPartyAuthExchange>,
    ) -> Self {
        Self {
            mesh_client: mesh_client.unwrap_or_default(),
            http_client: HttpDownstreamClient::new(),
            auth_exchange: auth_exchange.unwrap_or_default(),
            upstream_manager: None,
            config_cache: None,
        }
    }

    pub fn with_upstream(
        mut self,
        upstream_manager: Arc<UpstreamManager>,
        cache: InMemoryConfigCache,
    ) -> Self {
        self.upstream_manager = Some(upstream_manager);
        self.config_cache = Some(cache);
        self
    }

    pub fn execute_tool_call(
        &self,
        server_name: &str,
        tool: &ToolRecord,
        arguments: &JsonValue,
        user_token: Option<&str>,
        extra_headers: Option<HashMap<String, String>>,
    ) -> Result<JsonValue, String> {
        let mut headers = extra_headers.unwrap_or_default();
        let protocol = tool.protocol;

        if protocol == ProtocolType::ExternalMcp {
            let um = self
                .upstream_manager
                .as_ref()
                .ok_or_else(|| "Upstream manager not configured for ExternalMcp".to_string())?;

            let server_record = if let Some(cache) = &self.config_cache {
                cache
                    .get_server_arc(server_name)
                    .ok_or_else(|| format!("Server '{}' not found in config cache", server_name))?
            } else {
                return Err("Config cache not configured for ExternalMcp".to_string());
            };

            let raw_result = um.call_tool(&server_record, &tool.name, arguments)?;

            // 1. If result.get("isError") == Some(true), returns Err(extracted_error_text)
            if raw_result
                .get("isError")
                .and_then(|x| x.as_bool())
                .unwrap_or(false)
            {
                let err_msg =
                    if let Some(content) = raw_result.get("content").and_then(|c| c.as_array()) {
                        let texts: Vec<String> = content
                            .iter()
                            .filter_map(|item| {
                                item.get("text")
                                    .and_then(|t| t.as_str())
                                    .map(|s| s.to_string())
                            })
                            .collect();
                        if !texts.is_empty() {
                            texts.join("\n")
                        } else {
                            raw_result.to_string_compact()
                        }
                    } else {
                        raw_result.to_string_compact()
                    };
                return Err(err_msg);
            }

            // 2. If result.get("structuredContent") is present and non-null, returns it directly as JsonValue
            if let Some(structured) = raw_result.get("structuredContent") {
                if !structured.is_null() {
                    return Ok(structured.clone());
                }
            }

            // 3. Otherwise inspects result.get("content") array:
            // if a single "type": "text" block is valid JSON, parses and returns it;
            // otherwise returns json!({ "text": combined_text, "content": content_arr })
            if let Some(content_arr) = raw_result.get("content").and_then(|c| c.as_array()) {
                if content_arr.len() == 1 {
                    if let Some(first) = content_arr.first() {
                        if first.get("type").and_then(|t| t.as_str()) == Some("text") {
                            if let Some(text_str) = first.get("text").and_then(|t| t.as_str()) {
                                if let Ok(parsed) = JsonValue::parse(text_str) {
                                    return Ok(parsed);
                                }
                            }
                        }
                    }
                }

                let mut combined_text = String::new();
                for item in content_arr {
                    if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                        if !combined_text.is_empty() {
                            combined_text.push('\n');
                        }
                        combined_text.push_str(text);
                    }
                }
                return Ok(json!({
                    "text": combined_text,
                    "content": JsonValue::Array(content_arr.clone())
                }));
            }

            return Ok(raw_result);
        }

        let wire_payload: Vec<u8> = match protocol {
            ProtocolType::ThirdParty => {
                if let Some(tok) = user_token {
                    let provider = tool
                        .metadata
                        .get("provider")
                        .and_then(|x| x.as_str())
                        .unwrap_or("jira");
                    if let Some(oauth_token) = self.auth_exchange.exchange_token(tok, provider) {
                        headers.insert(
                            "Authorization".to_string(),
                            format!("Bearer {}", oauth_token),
                        );
                    }
                }
                arguments.to_string_compact().into_bytes()
            }
            ProtocolType::Grpc => {
                headers.insert(
                    "content-type".to_string(),
                    "application/grpc+proto".to_string(),
                );
                Self::serialize_protobuf(&tool.name, arguments)
            }
            ProtocolType::TChannel => {
                headers.insert("as".to_string(), "thrift".to_string());
                Self::serialize_thrift(&tool.name, arguments)
            }
            ProtocolType::NativeMcp => {
                let mcp_call = json!({
                    "method": "tools/call",
                    "params": {
                        "name": tool.name.clone(),
                        "arguments": arguments.clone()
                    }
                });
                mcp_call.to_string_compact().into_bytes()
            }
            ProtocolType::Http => {
                headers.insert("content-type".to_string(), "application/json".to_string());
                arguments.to_string_compact().into_bytes()
            }
            ProtocolType::ExternalMcp => unreachable!(),
        };

        let method_name = if tool.downstream_method.is_empty() {
            tool.name.as_str()
        } else {
            tool.downstream_method.as_str()
        };

        let resp_bytes = if cfg!(feature = "transport-http")
            && protocol == ProtocolType::Http
            && !tool.downstream_endpoint.is_empty()
            && tool.downstream_endpoint.starts_with("http://")
        {
            self.http_client
                .post_json(&tool.downstream_endpoint, &wire_payload, Some(&headers))?
        } else {
            self.mesh_client.execute_downstream(
                server_name,
                method_name,
                protocol,
                &wire_payload,
                Some(headers),
                5000,
            )?
        };

        let resp_str = String::from_utf8(resp_bytes)
            .map_err(|e| format!("Invalid UTF-8 from downstream: {}", e))?;
        JsonValue::parse(&resp_str)
    }

    /// In-memory gRPC length-prefixed binary envelope framing (1-byte compression flag + 4-byte BE length + method + JSON payload).
    pub fn serialize_protobuf(method: &str, args: &JsonValue) -> Vec<u8> {
        let body = args.to_string_compact().into_bytes();
        let method_bytes = method.as_bytes();
        let total_len = (1 + method_bytes.len() + body.len()) as u32;
        let mut frame = Vec::with_capacity(5 + total_len as usize);
        frame.push(0x00); // uncompressed flag
        frame.extend_from_slice(&total_len.to_be_bytes());
        frame.push((method_bytes.len() & 0xFF) as u8);
        frame.extend_from_slice(method_bytes);
        frame.extend_from_slice(&body);
        frame
    }

    /// In-memory Thrift TBinaryProtocol strict-version envelope framing (`0x80 0x01 0x00 0x01` + method + JSON payload).
    pub fn serialize_thrift(method: &str, args: &JsonValue) -> Vec<u8> {
        let body = args.to_string_compact().into_bytes();
        let method_bytes = method.as_bytes();
        let mut frame = Vec::with_capacity(5 + method_bytes.len() + body.len());
        frame.extend_from_slice(&[0x80, 0x01, 0x00, 0x01]);
        frame.push((method_bytes.len() & 0xFF) as u8);
        frame.extend_from_slice(method_bytes);
        frame.extend_from_slice(&body);
        frame
    }
}
