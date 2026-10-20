//! `aifx` & `omni` CLI - Uber's CLI for agentic operations and Code Mode tool execution.
//! Commands:
//! - `aifx mcp list [--format table|json]`
//! - `aifx mcp search <query> [--format table|json]`
//! - `aifx mcp call <server> <tool> [--args '<json>'] [--project '<fields>'] [--output <path>]`
//! - `aifx mcp bench [--iterations <N>]` (High-speed Rust data plane micro-benchmark)

use crate::control_plane::registry::MCPRegistry;
use crate::data_plane::cache::InMemoryConfigCache;
use crate::data_plane::gateway::MCPProxyGateway;
use crate::json::JsonValue;
use crate::models::{RPCRequest, ToolStatus};
use crate::server::get_gateway_context;
use std::fs;
use std::time::Instant;

pub struct AifxCLI {
    pub registry: MCPRegistry,
    pub cache: InMemoryConfigCache,
    pub gateway: MCPProxyGateway,
    pub gateway_url: String,
}

impl AifxCLI {
    pub fn new(registry: Option<MCPRegistry>, gateway_url: Option<&str>) -> Self {
        let registry = registry.unwrap_or_else(|| get_gateway_context().registry.clone());
        let cache = InMemoryConfigCache::new(registry.clone(), 5.0);
        let gateway = MCPProxyGateway::new(cache.clone(), None);
        Self {
            registry,
            cache,
            gateway,
            gateway_url: gateway_url.unwrap_or("http://localhost:8080").to_string(),
        }
    }

    pub fn list_servers(&self, format_type: &str) -> String {
        self.cache.refresh_now();
        let servers = self.cache.list_servers();

        if format_type == "json" {
            let mut out = Vec::with_capacity(servers.len());
            for s in servers {
                out.push(json!({
                    "name": s.name.clone(),
                    "displayName": s.display_name.clone(),
                    "protocol": s.protocol.as_str(),
                    "status": s.status.as_str(),
                    "enabledTools": s.enabled_tools_count() as i64,
                    "totalTools": s.tools.len() as i64,
                }));
            }
            return JsonValue::Array(out).to_string_pretty();
        }

        let mut lines = vec![
            format!(
                "{:<25} {:<12} {:<10} {:<15} {:<35}",
                "SERVER NAME", "PROTOCOL", "STATUS", "ENABLED/TOTAL", "DESCRIPTION"
            ),
            "-".repeat(100),
        ];

        for s in servers {
            let enabled = s.enabled_tools_count();
            let ratio = format!("{}/{}", enabled, s.tools.len());
            let desc_short: String = s.description.chars().take(33).collect();
            lines.push(format!(
                "{:<25} {:<12} {:<10} {:<15} {:<35}",
                s.name,
                s.protocol.as_str(),
                s.status.as_str(),
                ratio,
                desc_short
            ));
        }
        lines.join("\n")
    }

    pub fn search_tools(&self, query: &str, format_type: &str) -> String {
        self.cache.refresh_now();
        let servers = self.cache.list_servers();
        let tokens: Vec<String> = query
            .to_lowercase()
            .split_whitespace()
            .map(String::from)
            .collect();

        let mut results = Vec::new();
        for s in servers {
            for t in s.tools.values() {
                let mut score = 0usize;
                let search_text =
                    format!("{} {} {}", s.name, t.name, t.description).to_lowercase();
                for tok in &tokens {
                    if search_text.contains(tok) {
                        score += 1;
                    }
                }
                if score > 0 || tokens.is_empty() {
                    results.push(json!({
                        "server": s.name.clone(),
                        "tool": t.name.clone(),
                        "status": t.status.as_str(),
                        "protocol": t.protocol.as_str(),
                        "description": t.description.clone(),
                    }));
                }
            }
        }

        if format_type == "json" {
            return JsonValue::Array(results).to_string_pretty();
        }

        let mut lines = vec![
            format!(
                "{:<22} {:<28} {:<10} {:<10} {:<40}",
                "SERVER", "TOOL", "STATUS", "PROTOCOL", "DESCRIPTION"
            ),
            "-".repeat(115),
        ];

        for r in results {
            let srv = r.get("server").and_then(|x| x.as_str()).unwrap_or("");
            let tl = r.get("tool").and_then(|x| x.as_str()).unwrap_or("");
            let st = r.get("status").and_then(|x| x.as_str()).unwrap_or("");
            let pr = r.get("protocol").and_then(|x| x.as_str()).unwrap_or("");
            let desc: String = r
                .get("description")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .chars()
                .take(38)
                .collect();
            lines.push(format!(
                "{:<22} {:<28} {:<10} {:<10} {:<40}",
                srv, tl, st, pr, desc
            ));
        }
        lines.join("\n")
    }

    pub fn call_tool(
        &self,
        server_name: &str,
        tool_name: &str,
        arguments_json: &str,
        projection: Option<&str>,
        output_file: Option<&str>,
        actor: &str,
        token: Option<&str>,
    ) -> String {
        self.cache.refresh_now();
        let mut args = if arguments_json.trim().is_empty() {
            JsonValue::object()
        } else {
            match JsonValue::parse(arguments_json) {
                Ok(v) => v,
                Err(e) => {
                    return json!({
                        "error": format!("Invalid JSON in --args: {}", e)
                    })
                    .to_string_pretty()
                }
            }
        };

        if let Some(proj_str) = projection {
            let paths: Vec<JsonValue> = proj_str
                .split(',')
                .map(|p| p.trim())
                .filter(|p| !p.is_empty())
                .map(JsonValue::from)
                .collect();
            if !paths.is_empty() {
                args.insert("_projection", JsonValue::Array(paths));
            }
        }

        let req = RPCRequest::new(
            1i64,
            "tools/call",
            json!({
                "name": tool_name,
                "arguments": args,
            }),
        );

        let resp = self
            .gateway
            .handle_request(server_name, &req, actor, token);
        let out_str = resp.to_json().to_string_pretty();

        if let Some(out_path) = output_file {
            if !out_path.is_empty() {
                let _ = fs::write(out_path, &out_str);
                return format!("Written response ({} bytes) to {}", out_str.len(), out_path);
            }
        }

        out_str
    }

    pub fn run_benchmark(&self, iterations: usize) -> String {
        self.cache.refresh_now();
        let n = iterations.max(100);
        let req = RPCRequest::new(
            1i64,
            "tools/call",
            json!({
                "name": "GetDriverLocation",
                "arguments": {
                    "driver_id": "drv_102",
                    "_projection": ["data.driver.name", "data.driver.vehicle.license_plate"]
                }
            }),
        );

        let t0 = Instant::now();
        for _ in 0..n {
            let _ = self.gateway.handle_request(
                "driverdispatchservice",
                &req,
                "agent",
                Some("uber_token_bench"),
            );
        }
        let elapsed = t0.elapsed();
        let total_ms = elapsed.as_secs_f64() * 1000.0;
        let avg_us = (elapsed.as_nanos() as f64 / n as f64) / 1000.0;
        let rps = (n as f64) / elapsed.as_secs_f64().max(1e-9);

        json!({
            "benchmark": "omni-mcp-rust-data-plane",
            "iterations": n as i64,
            "total_ms": (total_ms * 100.0).round() / 100.0,
            "avg_latency_us": (avg_us * 100.0).round() / 100.0,
            "throughput_rps": rps.round() as i64,
            "features_active": ["CoW_ArcCache", "WireTranslation_gRPC", "ResponseProjection", "PIIRedaction", "ABAC_Charter"]
        })
        .to_string_pretty()
    }
}

pub fn run_aifx_cli(args: &[String]) -> i32 {
    if args.is_empty() || args[0] == "--help" || args[0] == "-h" {
        print_help();
        return 0;
    }

    if args[0] != "mcp" {
        print_help();
        return 1;
    }

    if args.len() < 2 {
        print_help();
        return 1;
    }

    let cli = AifxCLI::new(None, None);
    let subcmd = args[1].as_str();

    match subcmd {
        "list" => {
            let mut format_type = "table";
            let mut i = 2;
            while i < args.len() {
                if args[i] == "--format" && i + 1 < args.len() {
                    format_type = args[i + 1].as_str();
                    i += 2;
                } else {
                    i += 1;
                }
            }
            println!("{}", cli.list_servers(format_type));
            0
        }
        "search" => {
            let mut query = "";
            let mut format_type = "table";
            let mut i = 2;
            while i < args.len() {
                if args[i] == "--format" && i + 1 < args.len() {
                    format_type = args[i + 1].as_str();
                    i += 2;
                } else if !args[i].starts_with('-') && query.is_empty() {
                    query = args[i].as_str();
                    i += 1;
                } else {
                    i += 1;
                }
            }
            println!("{}", cli.search_tools(query, format_type));
            0
        }
        "call" => {
            if args.len() < 4 {
                eprintln!("Usage: aifx mcp call <server> <tool> [--args '<json>'] [--project '<fields>'] [--output <path>]");
                return 1;
            }
            let server = &args[2];
            let tool = &args[3];
            let mut json_args = "{}";
            let mut project: Option<&str> = None;
            let mut output: Option<&str> = None;
            let mut actor = "agent";
            let mut token = "uber_token_default";

            let mut i = 4;
            while i < args.len() {
                match args[i].as_str() {
                    "--args" if i + 1 < args.len() => {
                        json_args = &args[i + 1];
                        i += 2;
                    }
                    "--project" if i + 1 < args.len() => {
                        project = Some(&args[i + 1]);
                        i += 2;
                    }
                    "--output" | "-o" if i + 1 < args.len() => {
                        output = Some(&args[i + 1]);
                        i += 2;
                    }
                    "--actor" if i + 1 < args.len() => {
                        actor = &args[i + 1];
                        i += 2;
                    }
                    "--token" if i + 1 < args.len() => {
                        token = &args[i + 1];
                        i += 2;
                    }
                    _ => i += 1,
                }
            }

            let res = cli.call_tool(
                server,
                tool,
                json_args,
                project,
                output,
                actor,
                Some(token),
            );
            println!("{}", res);
            0
        }
        "bench" => {
            let mut iterations = 10_000usize;
            let mut i = 2;
            while i < args.len() {
                if args[i] == "--iterations" && i + 1 < args.len() {
                    iterations = args[i + 1].parse().unwrap_or(10_000);
                    i += 2;
                } else {
                    i += 1;
                }
            }
            println!("{}", cli.run_benchmark(iterations));
            0
        }
        _ => {
            let _ = ToolStatus::Enabled;
            print_help();
            1
        }
    }
}

fn print_help() {
    println!(
        "omni / aifx - High-Performance Rust CLI for Agentic Operations & Code Mode MCP Execution\n\n\
         USAGE:\n\
         \x20 aifx mcp list [--format table|json]\n\
         \x20 aifx mcp search [query] [--format table|json]\n\
         \x20 aifx mcp call <server> <tool> [--args '<json>'] [--project '<fields>'] [--output <path>] [--actor <actor>]\n\
         \x20 aifx mcp bench [--iterations <N>]"
    );
}
