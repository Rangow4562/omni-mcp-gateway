//! Protocol Translators - Converts MCP JSON-RPC payloads into in-memory HTTP, gRPC (Protobuf length-prefixed), and TChannel (Thrift) wire envelope frames.

use crate::control_plane::third_party::ThirdPartyAuthExchange;
use crate::data_plane::muttley::MuttleyMeshClient;
use crate::json::JsonValue;
use crate::models::{ProtocolType, ToolRecord};
use std::collections::HashMap;

#[derive(Clone, Default)]
pub struct ProtocolTranslator {
    pub mesh_client: MuttleyMeshClient,
    pub auth_exchange: ThirdPartyAuthExchange,
}

impl ProtocolTranslator {
    pub fn new(
        mesh_client: Option<MuttleyMeshClient>,
        auth_exchange: Option<ThirdPartyAuthExchange>,
    ) -> Self {
        Self {
            mesh_client: mesh_client.unwrap_or_default(),
            auth_exchange: auth_exchange.unwrap_or_default(),
        }
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
        };

        let method_name = if tool.downstream_method.is_empty() {
            tool.name.as_str()
        } else {
            tool.downstream_method.as_str()
        };

        let resp_bytes = self.mesh_client.execute_downstream(
            server_name,
            method_name,
            protocol,
            &wire_payload,
            Some(headers),
            5000,
        )?;

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
