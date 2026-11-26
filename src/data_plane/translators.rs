//! Protocol Translators - Converts MCP JSON-RPC payloads into in-memory HTTP, gRPC (Protobuf length-prefixed), and TChannel (Thrift) wire envelope frames.
//! When the `transport-http` feature is enabled and a tool provides a live `http://` downstream endpoint,
//! dispatches live network I/O via `HttpDownstreamClient`.

use crate::control_plane::third_party::ThirdPartyAuthExchange;
use crate::data_plane::cache::InMemoryConfigCache;
use crate::data_plane::downstream_grpc::GrpcDownstreamClient;
use crate::data_plane::downstream_http::HttpDownstreamClient;
use crate::data_plane::muttley::MuttleyMeshClient;
use crate::json::JsonValue;
use crate::models::{ProtocolType, ToolRecord};
use crate::storage::FileBlobStore;
use crate::upstream::UpstreamManager;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

thread_local! {
    static ACTIVE_CANCEL_FLAG: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
}

struct ActiveCancelFlagGuard {
    prev: Option<Arc<AtomicBool>>,
}

impl Drop for ActiveCancelFlagGuard {
    fn drop(&mut self) {
        let prev = self.prev.take();
        ACTIVE_CANCEL_FLAG.with(|cell| {
            *cell.borrow_mut() = prev;
        });
    }
}

/// Runs `f` with `flag` installed as the thread-local cooperative cancellation flag
/// for Wasm ABI v2 and downstream tool execution.
pub fn with_active_cancel_flag<F, R>(flag: Arc<AtomicBool>, f: F) -> R
where
    F: FnOnce() -> R,
{
    let prev = ACTIVE_CANCEL_FLAG.with(|cell| cell.borrow_mut().replace(flag));
    let _guard = ActiveCancelFlagGuard { prev };
    f()
}

/// Returns the active cooperative cancellation flag for the current execution thread, if any.
pub fn current_cancel_flag() -> Option<Arc<AtomicBool>> {
    ACTIVE_CANCEL_FLAG.with(|cell| cell.borrow().clone())
}

#[derive(Clone, Default)]
pub struct ProtocolTranslator {
    pub mesh_client: MuttleyMeshClient,
    pub http_client: HttpDownstreamClient,
    pub grpc_client: GrpcDownstreamClient,
    pub auth_exchange: ThirdPartyAuthExchange,
    pub upstream_manager: Option<Arc<UpstreamManager>>,
    pub config_cache: Option<InMemoryConfigCache>,
    pub file_store: Option<FileBlobStore>,
}

impl ProtocolTranslator {
    pub fn new(
        mesh_client: Option<MuttleyMeshClient>,
        auth_exchange: Option<ThirdPartyAuthExchange>,
    ) -> Self {
        Self {
            mesh_client: mesh_client.unwrap_or_default(),
            http_client: HttpDownstreamClient::new(),
            grpc_client: GrpcDownstreamClient::new(),
            auth_exchange: auth_exchange.unwrap_or_default(),
            upstream_manager: None,
            config_cache: None,
            file_store: None,
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

    pub fn with_file_store(mut self, file_store: FileBlobStore) -> Self {
        self.file_store = Some(file_store);
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
        #[allow(unused_variables)]
        let caller_actor = headers
            .remove("x-omni-caller-actor")
            .unwrap_or_else(|| "agent".to_string());
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

        if protocol == ProtocolType::WasmPlugin {
            #[cfg(not(feature = "sandbox-wasm"))]
            {
                return Err(format!(
                    "WasmPlugin tool '{}' requires the 'sandbox-wasm' feature",
                    tool.name
                ));
            }
            #[cfg(feature = "sandbox-wasm")]
            {
                let b64 = tool
                    .wasm_module_base64
                    .as_deref()
                    .or_else(|| {
                        tool.metadata
                            .get("wasm_module_base64")
                            .and_then(|v| v.as_str())
                    })
                    .ok_or_else(|| {
                        format!(
                            "WasmPlugin tool '{}' is missing 'wasm_module_base64' module payload",
                            tool.name
                        )
                    })?;
                let wasm_bytes = crate::data_plane::codemode::decode_wasm_base64(b64)?;
                let max_fuel = tool
                    .metadata
                    .get("wasm_max_fuel")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(250_000);
                let max_mem = tool
                    .metadata
                    .get("wasm_max_memory_bytes")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize)
                    .unwrap_or(16 * 1024 * 1024);
                let executor =
                    crate::data_plane::codemode::WasmSandboxExecutor::new(max_fuel, max_mem);

                // Governed host tool callback for Wasm plugins invoking other tools via `omni::call_tool`
                let translator_clone = self.clone();
                let tok_owned = user_token.map(|s| s.to_string());
                let actor_owned = caller_actor.clone();
                let cb: crate::data_plane::codemode::WasmToolCallback = Arc::new(
                    move |target_srv: &str, target_tool: &str, target_args: &JsonValue| {
                        let cache = translator_clone.config_cache.as_ref().ok_or_else(|| {
                            "Config cache unavailable for Wasm host bridge".to_string()
                        })?;
                        let srv = cache.get_server_arc(target_srv).ok_or_else(|| {
                            format!("Wasm host bridge: server '{}' not found", target_srv)
                        })?;
                        let t = srv.tools.get(target_tool).ok_or_else(|| {
                            format!(
                                "Wasm host bridge: tool '{}' not found on '{}'",
                                target_tool, target_srv
                            )
                        })?;
                        if t.status != crate::models::ToolStatus::Enabled {
                            return Err(format!(
                                "Wasm host bridge: tool '{}.{}' is not enabled",
                                target_srv, target_tool
                            ));
                        }
                        if t.is_mutation {
                            return Err(format!(
                                "Wasm host bridge: mutating tool '{}.{}' requires explicit HITL approval",
                                target_srv, target_tool
                            ));
                        }
                        let (allowed, reason) =
                            crate::data_plane::security::SecurityCharterEngine::evaluate_access_with_args(
                                &srv,
                                t,
                                &actor_owned,
                                tok_owned.as_deref(),
                                target_args,
                            );
                        if !allowed {
                            return Err(reason
                                .unwrap_or_else(|| "Unauthorized by Charter Policy".to_string()));
                        }
                        if let Err(violations) =
                            crate::models::validate_tool_schema(&t.input_schema, target_args)
                        {
                            let first_msg = violations
                                .first()
                                .map(|v| v.message.clone())
                                .unwrap_or_else(|| "invalid arguments".to_string());
                            return Err(format!("Schema validation failed: {}", first_msg));
                        }
                        let mut nested_hdrs = HashMap::new();
                        nested_hdrs.insert("x-omni-caller-actor".to_string(), actor_owned.clone());
                        translator_clone.execute_tool_call(
                            target_srv,
                            t,
                            target_args,
                            tok_owned.as_deref(),
                            Some(nested_hdrs),
                        )
                    },
                );

                let mut attached_files = Vec::new();
                if let Some(ref fs) = self.file_store {
                    let file_ids = Self::collect_file_ids(arguments);
                    for fid in file_ids {
                        if let Some(meta) = fs.get_metadata(&fid) {
                            let fs_clone = fs.clone();
                            attached_files.push(crate::data_plane::codemode::WasmAttachedFile {
                                file_id: meta.file_id.clone(),
                                size_bytes: meta.size_bytes,
                                reader: Arc::new(move |id: &str, offset: u64, len: usize| {
                                    fs_clone.read_range(id, offset, len)
                                }),
                            });
                        }
                    }
                }
                let cancel_flag = current_cancel_flag();
                let out = executor.execute_wasm_abi_v2(
                    &wasm_bytes,
                    arguments,
                    Some(cb),
                    attached_files,
                    cancel_flag,
                );
                if let Some(err) = out.error {
                    return Err(err);
                }
                return Ok(out.result);
            }
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
                // Live gRPC/Connect clients set their own wire Content-Type; only set mesh header below.
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
            ProtocolType::ExternalMcp | ProtocolType::WasmPlugin => unreachable!(),
        };

        let method_name = if tool.downstream_method.is_empty() {
            tool.name.as_str()
        } else {
            tool.downstream_method.as_str()
        };

        let is_live_http_url = tool.downstream_endpoint.starts_with("http://")
            || tool.downstream_endpoint.starts_with("https://");
        let is_live_grpc_url = is_live_http_url
            || tool.downstream_endpoint.starts_with("grpc://")
            || tool.downstream_endpoint.starts_with("grpcs://")
            || tool.downstream_endpoint.starts_with("h2c://");

        let resp_bytes = if cfg!(feature = "transport-http")
            && protocol == ProtocolType::Http
            && is_live_http_url
        {
            if let Some(ref fs) = self.file_store {
                if let Some((field_name, file_id)) = Self::find_primary_file_arg(tool, arguments) {
                    if fs.get_metadata(&file_id).is_some() {
                        let timeout_sec = tool.timeout_ms.map(|ms| ms.div_ceil(1000));
                        let streamed = self.http_client.post_stream_file(
                            &tool.downstream_endpoint,
                            fs,
                            &file_id,
                            Some(&field_name),
                            Some(&headers),
                            timeout_sec,
                        )?;
                        let resp_str = String::from_utf8(streamed)
                            .map_err(|e| format!("Invalid UTF-8 from downstream: {}", e))?;
                        return JsonValue::parse(&resp_str);
                    }
                }
            }
            let http_verb = tool
                .http_method
                .as_deref()
                .or_else(|| tool.metadata.get("http_method").and_then(|v| v.as_str()))
                .unwrap_or("POST");
            self.http_client.request(
                http_verb,
                &tool.downstream_endpoint,
                arguments,
                Some(&headers),
            )?
        } else if (cfg!(feature = "transport-grpc") || cfg!(feature = "transport-http"))
            && protocol == ProtocolType::Grpc
            && is_live_grpc_url
        {
            if let Some(ref fs) = self.file_store {
                if let Some((_field_name, file_id)) = Self::find_primary_file_arg(tool, arguments) {
                    if let Some(file_meta) = fs.get_metadata(&file_id) {
                        let streamed = self.grpc_client.invoke_grpc_with_file_metadata(
                            &tool.downstream_endpoint,
                            method_name,
                            arguments,
                            &file_meta,
                            Some(&headers),
                        )?;
                        let resp_str = String::from_utf8(streamed)
                            .map_err(|e| format!("Invalid UTF-8 from downstream: {}", e))?;
                        return JsonValue::parse(&resp_str);
                    }
                }
            }
            self.grpc_client.invoke(
                &tool.downstream_endpoint,
                method_name,
                arguments,
                Some(&headers),
            )?
        } else {
            if protocol == ProtocolType::Grpc {
                headers.insert(
                    "content-type".to_string(),
                    "application/grpc+proto".to_string(),
                );
            }
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

    #[cfg(feature = "sandbox-wasm")]
    fn collect_file_ids(val: &JsonValue) -> Vec<String> {
        fn walk(v: &JsonValue, out: &mut Vec<String>) {
            match v {
                JsonValue::Object(map) => {
                    for key in ["fileId", "file_id"] {
                        if let Some(fid) = v.get(key).and_then(|x| x.as_str()) {
                            if !fid.is_empty() && !out.iter().any(|existing| existing == fid) {
                                out.push(fid.to_string());
                            }
                        }
                    }
                    for (_, child) in map {
                        walk(child, out);
                    }
                }
                JsonValue::Array(arr) => {
                    for child in arr {
                        walk(child, out);
                    }
                }
                _ => {}
            }
        }
        let mut out = Vec::new();
        walk(val, &mut out);
        out
    }

    #[allow(dead_code)]
    fn find_primary_file_arg(tool: &ToolRecord, arguments: &JsonValue) -> Option<(String, String)> {
        let schema_props =
            crate::extension::file_uploads::find_mcp_file_properties(&tool.input_schema);
        for prop in schema_props {
            if let Some(val) = arguments.get(&prop) {
                if let Some(fid) = val
                    .get("fileId")
                    .or_else(|| val.get("file_id"))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                {
                    return Some((prop, fid.to_string()));
                }
            }
        }
        if let Some(obj) = arguments.as_object() {
            for (k, v) in obj {
                if let Some(fid) = v
                    .get("fileId")
                    .or_else(|| v.get("file_id"))
                    .and_then(|x| x.as_str())
                    .filter(|s| !s.is_empty())
                {
                    return Some((k.clone(), fid.to_string()));
                }
            }
            if let Some(fid) = arguments
                .get("fileId")
                .or_else(|| arguments.get("file_id"))
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
            {
                return Some(("file".to_string(), fid.to_string()));
            }
        }
        None
    }
}
