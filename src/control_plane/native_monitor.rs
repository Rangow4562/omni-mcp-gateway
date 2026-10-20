//! Native MCPFx Heartbeat Monitor - Detects native MCP servers emitting heartbeat signals and discovers tools.

use crate::json::JsonValue;
use crate::logging::now_epoch_secs;
use crate::models::{ProtocolType, ServerRecord, ToolRecord, ToolStatus};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, RwLock};

pub type ListToolsCallback = Arc<dyn Fn(&str) -> Vec<JsonValue> + Send + Sync>;

#[derive(Clone, Debug)]
struct HeartbeatMeta {
    pub display_name: String,
    pub host: String,
    pub port: u16,
    pub owning_team: String,
    pub timestamp: f64,
}

#[derive(Clone)]
pub struct MCPFxHeartbeatMonitor {
    heartbeats: Arc<RwLock<HashMap<String, HeartbeatMeta>>>,
    list_tools_client: ListToolsCallback,
}

impl Default for MCPFxHeartbeatMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl MCPFxHeartbeatMonitor {
    pub fn new() -> Self {
        Self {
            heartbeats: Arc::new(RwLock::new(HashMap::new())),
            list_tools_client: Arc::new(Self::default_mock_list_tools),
        }
    }

    pub fn with_client(list_tools_client: ListToolsCallback) -> Self {
        Self {
            heartbeats: Arc::new(RwLock::new(HashMap::new())),
            list_tools_client,
        }
    }

    pub fn emit_heartbeat(
        &self,
        server_name: &str,
        host: &str,
        port: u16,
        owning_team: &str,
        display_name: &str,
    ) {
        let mut guard = self.heartbeats.write().unwrap();
        guard.insert(
            server_name.to_string(),
            HeartbeatMeta {
                display_name: if display_name.is_empty() {
                    server_name.to_string()
                } else {
                    display_name.to_string()
                },
                host: host.to_string(),
                port,
                owning_team: owning_team.to_string(),
                timestamp: now_epoch_secs(),
            },
        );
    }

    pub fn scan_active_native_servers(&self, max_stale_seconds: f64) -> Vec<ServerRecord> {
        let now = now_epoch_secs();
        let snapshot: Vec<(String, HeartbeatMeta)> = {
            let guard = self.heartbeats.read().unwrap();
            guard.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
        };

        let mut discovered = Vec::new();
        for (name, meta) in snapshot {
            if now - meta.timestamp > max_stale_seconds {
                continue;
            }

            let raw_tools = (self.list_tools_client)(&name);
            let mut tools_map = BTreeMap::new();

            for rt in raw_tools {
                let t_name = rt
                    .get("name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("native_tool")
                    .to_string();
                let t_desc = rt
                    .get("description")
                    .and_then(|x| x.as_str())
                    .map(String::from)
                    .unwrap_or_else(|| format!("Native MCP tool {}", t_name));
                let t_schema = rt
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}}));

                let mut tool = ToolRecord::new(t_name.clone(), t_desc, t_schema);
                tool.status = ToolStatus::Disabled; // Disabled by default
                tool.protocol = ProtocolType::NativeMcp;
                tool.downstream_endpoint = format!("http://{}:{}/mcp", meta.host, meta.port);
                tool.downstream_method = t_name.clone();
                tools_map.insert(t_name, tool);
            }

            let mut srv = ServerRecord::new(
                name.clone(),
                meta.display_name,
                format!(
                    "Native MCP server on {}:{} (discovered via MCPFx heartbeat)",
                    meta.host, meta.port
                ),
                meta.owning_team,
            );
            srv.protocol = ProtocolType::NativeMcp;
            srv.status = ToolStatus::Disabled;
            srv.tools = tools_map;
            srv.heartbeat_timestamp = Some(meta.timestamp);
            srv.is_native = true;
            discovered.push(srv);
        }

        discovered
    }

    fn default_mock_list_tools(server_name: &str) -> Vec<JsonValue> {
        vec![json!({
            "name": format!("{}_query", server_name),
            "description": format!("Native high-throughput query on {}", server_name),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search or filter query"}
                },
                "required": ["query"]
            }
        })]
    }
}
