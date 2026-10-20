//! AutoCrawler - Cadence-powered distributed workflow engine for discovering APIs and native servers.

use crate::control_plane::description_gen::ToolDescriptionGenerator;
use crate::control_plane::idl_parser::{IDLParser, ServiceDefinition};
use crate::control_plane::native_monitor::MCPFxHeartbeatMonitor;
use crate::control_plane::registry::MCPRegistry;
use crate::json::JsonValue;
use crate::logging::now_epoch_secs;
use crate::models::{ServerRecord, ToolRecord, ToolStatus};
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

struct CrawlerState {
    idl_registry_store: BTreeMap<String, String>,
    last_run_timestamp: Option<f64>,
    run_count: u64,
}

#[derive(Clone)]
pub struct AutoCrawler {
    pub registry: MCPRegistry,
    pub native_monitor: MCPFxHeartbeatMonitor,
    pub description_gen: ToolDescriptionGenerator,
    state: Arc<RwLock<CrawlerState>>,
}

impl AutoCrawler {
    pub fn new(
        registry: MCPRegistry,
        native_monitor: Option<MCPFxHeartbeatMonitor>,
        description_gen: Option<ToolDescriptionGenerator>,
    ) -> Self {
        Self {
            registry,
            native_monitor: native_monitor.unwrap_or_default(),
            description_gen: description_gen.unwrap_or_default(),
            state: Arc::new(RwLock::new(CrawlerState {
                idl_registry_store: BTreeMap::new(),
                last_run_timestamp: None,
                run_count: 0,
            })),
        }
    }

    pub fn register_idl_source(&self, service_name: &str, idl_content: &str) {
        let mut guard = self.state.write().unwrap();
        guard
            .idl_registry_store
            .insert(service_name.to_string(), idl_content.to_string());
    }

    pub fn run_discovery_workflow(&self) -> JsonValue {
        let (run_count, timestamp, idl_sources) = {
            let mut guard = self.state.write().unwrap();
            guard.run_count += 1;
            let now = now_epoch_secs();
            guard.last_run_timestamp = Some(now);
            (
                guard.run_count,
                now,
                guard.idl_registry_store.clone(),
            )
        };

        let mut discovered_servers = 0usize;
        let mut discovered_tools = 0usize;

        // 1. Discover IDL-Backed Services
        for (_svc_key, content) in idl_sources {
            let parsed_services: Vec<ServiceDefinition> =
                if content.contains("syntax = \"proto")
                    || (content.contains("service ") && content.contains("rpc "))
                {
                    IDLParser::parse_protobuf(&content)
                } else if content.contains("namespace ") || content.contains("service ") {
                    IDLParser::parse_thrift(&content)
                } else {
                    Vec::new()
                };

            for svc_def in parsed_services {
                let virtual_server_name = svc_def.service_name.to_lowercase();
                let mut is_new_server = false;
                let mut server_record = match self.registry.get_server(&virtual_server_name) {
                    Some(s) => s,
                    None => {
                        is_new_server = true;
                        let desc = if svc_def.doc_comment.is_empty() {
                            format!("Virtual MCP Server for {}", svc_def.service_name)
                        } else {
                            svc_def.doc_comment.clone()
                        };
                        let mut srv = ServerRecord::new(
                            virtual_server_name.clone(),
                            svc_def.service_name.clone(),
                            desc,
                            format!("team-{}", virtual_server_name),
                        );
                        srv.protocol = svc_def.protocol;
                        srv.status = ToolStatus::Disabled;
                        srv
                    }
                };

                for method in &svc_def.methods {
                    if !server_record.tools.contains_key(&method.name) {
                        let input_schema = IDLParser::translate_to_json_schema(method);
                        let enriched_desc = self
                            .description_gen
                            .generate_description(&svc_def.service_name, method);

                        let mut tool_record =
                            ToolRecord::new(method.name.clone(), enriched_desc, input_schema);
                        tool_record.status = ToolStatus::Disabled;
                        tool_record.protocol = svc_def.protocol;
                        tool_record.downstream_endpoint =
                            format!("muttley://{}/{}", virtual_server_name, method.name);
                        tool_record.downstream_method = method.name.clone();

                        server_record.add_tool(tool_record);
                        discovered_tools += 1;
                    }
                }

                if is_new_server {
                    discovered_servers += 1;
                }
                self.registry.register_server(server_record);
            }
        }

        // 2. Discover Native Servers via MCPFx Heartbeats
        let native_servers = self.native_monitor.scan_active_native_servers(60.0);
        for ns in native_servers {
            if let Some(mut existing) = self.registry.get_server(&ns.name) {
                for (t_name, t_val) in ns.tools {
                    if !existing.tools.contains_key(&t_name) {
                        existing.add_tool(t_val);
                        discovered_tools += 1;
                    }
                }
                self.registry.register_server(existing);
            } else {
                discovered_servers += 1;
                discovered_tools += ns.tools.len();
                self.registry.register_server(ns);
            }
        }

        let total_registered = self.registry.list_servers(None).len();

        json!({
            "workflow_run_id": format!("cadence-run-{}", run_count),
            "timestamp": timestamp,
            "discovered_servers": discovered_servers as i64,
            "discovered_tools": discovered_tools as i64,
            "total_registered_servers": total_registered as i64,
        })
    }
}
