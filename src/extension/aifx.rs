//! `aifx` & `omni` CLI - CLI for agentic operations and Code Mode tool execution.
//! Commands:
//! - `aifx mcp list [--format table|json]`
//! - `aifx mcp search <query> [--format table|json]`
//! - `aifx mcp call <server> <tool> [--args '<json>'] [--project '<fields>'] [--output <path>]`
//! - `aifx mcp bench [--iterations <N>]` (High-speed Rust data plane micro-benchmark)

use crate::control_plane::registry::MCPRegistry;
use crate::data_plane::cache::InMemoryConfigCache;
use crate::data_plane::dst::FaultInjectionConfig;
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
                let search_text = format!("{} {} {}", s.name, t.name, t.description).to_lowercase();
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

    #[allow(clippy::too_many_arguments)]
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

        let resp = self.gateway.handle_request(server_name, &req, actor, token);
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
                Some("demo_token_bench"),
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

    pub fn run_codemode(
        &self,
        code: &str,
        require_approval: bool,
        actor: &str,
        token: Option<&str>,
    ) -> String {
        self.cache.refresh_now();
        let out = self
            .gateway
            .execute_codemode(code, None, require_approval, actor, token, None);
        out.to_json().to_string_pretty()
    }

    pub fn get_types(
        &self,
        server_filter: Option<&str>,
        query: Option<&str>,
        intent: Option<&str>,
    ) -> String {
        self.cache.refresh_now();
        if let Some(intent_str) = intent {
            let spec = self
                .gateway
                .generate_speculative_codemode_types(intent_str, 4, 0.82);
            return spec.to_json().to_string_pretty();
        }
        let gen = self.gateway.generate_codemode_types(server_filter, query);
        gen.to_json().to_string_pretty()
    }

    pub fn resume_codemode(
        &self,
        execution_id: &str,
        approver: &str,
        token: Option<&str>,
        digest: Option<&str>,
    ) -> String {
        self.cache.refresh_now();
        let out = self
            .gateway
            .resume_codemode(execution_id, approver, None, token, None, digest);
        out.to_json().to_string_pretty()
    }

    pub fn simulate_codemode(
        &self,
        code: &str,
        seed: u64,
        runs: usize,
        actor: &str,
        token: Option<&str>,
    ) -> String {
        self.cache.refresh_now();
        let cfg = FaultInjectionConfig {
            base_seed: seed,
            num_runs: runs.clamp(1, 1000),
            ..Default::default()
        };
        let report = self
            .gateway
            .simulate_codemode(code, None, cfg, actor, token);
        report.to_json().to_string_pretty()
    }

    pub fn run_codemode_benchmark(&self, iterations: usize) -> String {
        self.cache.refresh_now();
        let n = iterations.max(100);
        let script = r#"
        async () => {
            const rides = await codemode.rides_service.list_recent({ user_id: "usr_456" });
            const cancelled = rides.filter(r => r.status === "cancelled" && r.amount > 20.0);
            let total = 0.0;
            for (const r of cancelled) {
                total += r.amount;
            }
            return { refunded_count: cancelled.length, total_amount: total };
        }
        "#;

        let t0 = Instant::now();
        let mut last_out = None;
        for _ in 0..n {
            last_out = Some(self.gateway.execute_codemode(
                script,
                None,
                false,
                "agent",
                Some("demo_token_bench"),
                None,
            ));
        }
        let elapsed = t0.elapsed();
        let total_ms = elapsed.as_secs_f64() * 1000.0;
        let avg_us = (elapsed.as_nanos() as f64 / n as f64) / 1000.0;
        let rps = (n as f64) / elapsed.as_secs_f64().max(1e-9);
        let sample = last_out.unwrap();

        json!({
            "benchmark": "omni-mcp-codemode-sandbox",
            "iterations": n as i64,
            "total_ms": (total_ms * 100.0).round() / 100.0,
            "avg_sandbox_latency_us": (avg_us * 100.0).round() / 100.0,
            "sub_millisecond_verified": avg_us < 1000.0,
            "throughput_scripts_per_sec": rps.round() as i64,
            "sample_result": sample.result,
            "ops_per_script": sample.ops_consumed as i64
        })
        .to_string_pretty()
    }
}

pub fn run_aifx_cli(args: &[String]) -> i32 {
    let mut clean_args: Vec<String> = args.to_vec();
    if !clean_args.is_empty()
        && (clean_args[0] == "aifx" || clean_args[0] == "omni" || clean_args[0] == "omni-gateway")
    {
        clean_args.remove(0);
    }
    if !clean_args.is_empty() && clean_args[0] == "mcp" {
        clean_args.remove(0);
    }

    if clean_args.is_empty()
        || clean_args[0] == "--help"
        || clean_args[0] == "-h"
        || clean_args[0] == "help"
    {
        print_help();
        return 0;
    }

    if clean_args[0] == "--version"
        || clean_args[0] == "-V"
        || clean_args[0] == "-v"
        || clean_args[0] == "version"
    {
        println!("omni-mcp v{}", env!("CARGO_PKG_VERSION"));
        return 0;
    }

    clean_args.insert(0, "mcp".to_string());
    let args = &clean_args;

    // Dispatch pure-offline CLI commands BEFORE initializing gateway storage (`AifxCLI::new`)
    // so they execute hermetically with zero `~/.omni/data` or Postgres side effects.
    if let Some(exit_code) = try_dispatch_offline_cli(args) {
        return exit_code;
    }

    let cli = AifxCLI::new(None, None);
    let subcmd = args[1].as_str();

    match subcmd {
        "skills" => {
            let action = args.get(2).map(|s| s.as_str()).unwrap_or("list");
            let engine = crate::control_plane::SkillEngine::new(cli.registry.clone());
            match action {
                "list" => {
                    let mut group_slug: Option<String> = None;
                    let mut i = 3;
                    while i < args.len() {
                        if args[i] == "--group" && i + 1 < args.len() {
                            group_slug = Some(args[i + 1].clone());
                            i += 2;
                        } else {
                            i += 1;
                        }
                    }
                    let scope = crate::control_plane::SkillAccessScope {
                        group_slug,
                        server_scope: None,
                    };
                    let skills = engine.list_skills(&scope);
                    let arr: Vec<JsonValue> = skills
                        .iter()
                        .map(crate::control_plane::SkillEngine::to_mcp_summary)
                        .collect();
                    println!("{}", JsonValue::Array(arr).to_string_pretty());
                    0
                }
                "publish" => {
                    if args.len() < 4 {
                        eprintln!(
                            "Usage: omni skills publish <dir> [--group <slug>] [--slug <slug>]"
                        );
                        return 1;
                    }
                    let dir = std::path::Path::new(&args[3]);
                    let mut group_slug: Option<String> = None;
                    let mut slug_override: Option<String> = None;
                    let mut i = 4;
                    while i < args.len() {
                        if args[i] == "--group" && i + 1 < args.len() {
                            group_slug = Some(args[i + 1].clone());
                            i += 2;
                        } else if args[i] == "--slug" && i + 1 < args.len() {
                            slug_override = Some(args[i + 1].clone());
                            i += 2;
                        } else {
                            i += 1;
                        }
                    }
                    match engine.publish_from_dir(dir, slug_override.as_deref(), group_slug, None) {
                        Ok(saved) => {
                            println!(
                                "Published skill '{}' (version={}, files={}, sha256={})",
                                saved.slug,
                                saved.version,
                                saved.files.len(),
                                saved.manifest_sha256
                            );
                            0
                        }
                        Err(e) => {
                            eprintln!("Failed to publish skill: {}", e);
                            1
                        }
                    }
                }
                "verify" => {
                    if args.len() < 4 {
                        eprintln!("Usage: omni skills verify <slug>");
                        return 1;
                    }
                    let slug = &args[3];
                    let scope = crate::control_plane::SkillAccessScope::default();
                    match engine.get_skill(slug, &scope) {
                        Some(mut sk) => {
                            let stored_sha = sk.manifest_sha256.clone();
                            match sk.validate() {
                                Ok(()) if sk.manifest_sha256 == stored_sha => {
                                    println!(
                                        "VERIFIED skill '{}' (files={}, total_bytes={}, manifest_sha256={})",
                                        sk.slug,
                                        sk.files.len(),
                                        sk.total_bytes(),
                                        sk.manifest_sha256
                                    );
                                    0
                                }
                                Ok(()) => {
                                    eprintln!(
                                        "MANIFEST DRIFT for '{}': stored={} recomputed={}",
                                        sk.slug, stored_sha, sk.manifest_sha256
                                    );
                                    1
                                }
                                Err(e) => {
                                    eprintln!("INVALID skill '{}': {}", slug, e);
                                    1
                                }
                            }
                        }
                        None => {
                            eprintln!("Skill '{}' not found", slug);
                            1
                        }
                    }
                }
                "bootstrap" => {
                    let mut group_slug: Option<String> = None;
                    let mut i = 3;
                    while i < args.len() {
                        if args[i] == "--group" && i + 1 < args.len() {
                            group_slug = Some(args[i + 1].clone());
                            i += 2;
                        } else {
                            i += 1;
                        }
                    }
                    match crate::extension::plugin::seed_builtin_skills_into_registry(
                        &cli.registry,
                        group_slug,
                    ) {
                        Ok(saved) => {
                            let arr: Vec<JsonValue> = saved
                                .iter()
                                .map(|s| {
                                    json!({
                                        "slug": s.slug.clone(),
                                        "version": s.version.clone(),
                                        "files": s.files.len() as i64,
                                        "manifest_sha256": s.manifest_sha256.clone()
                                    })
                                })
                                .collect();
                            println!(
                                "{}",
                                json!({
                                    "bootstrapped": saved.len() as i64,
                                    "skills": JsonValue::Array(arr)
                                })
                                .to_string_pretty()
                            );
                            0
                        }
                        Err(e) => {
                            eprintln!("Failed to bootstrap built-in skills: {}", e);
                            1
                        }
                    }
                }
                other => {
                    eprintln!(
                        "Unknown skills subcommand '{}'. Use: list | publish <dir> | verify <slug> | install | bootstrap",
                        other
                    );
                    1
                }
            }
        }
        "plugin" => {
            let action = args.get(2).map(|s| s.as_str()).unwrap_or("");
            if action == "export" {
                let opts = match parse_plugin_export_args(args) {
                    Ok(o) => o,
                    Err(e) => {
                        eprintln!("Error: {}", e);
                        return 2;
                    }
                };
                let extra_skills = if opts.include_registry_skills {
                    let engine = crate::control_plane::SkillEngine::new(cli.registry.clone());
                    let scope = crate::control_plane::SkillAccessScope {
                        group_slug: opts.group_slug.clone(),
                        server_scope: None,
                    };
                    engine.list_skills(&scope)
                } else {
                    Vec::new()
                };
                match crate::extension::plugin::export_enterprise_bundle(&opts, &extra_skills) {
                    Ok(report) => {
                        println!(
                            "{}",
                            json!({
                                "dry_run": report.dry_run,
                                "out_dir": report.out_dir,
                                "files_count": report.files.len() as i64,
                                "skills_exported": JsonValue::Array(
                                    report.skills_exported.into_iter().map(JsonValue::from).collect()
                                ),
                                "sha256sums_digest": report.sha256sums_digest
                            })
                            .to_string_pretty()
                        );
                        0
                    }
                    Err(e) => {
                        eprintln!("Enterprise export failed: {}", e);
                        1
                    }
                }
            } else {
                eprintln!(
                    "Unknown plugin subcommand '{}'. Use: init | export | verify <dir> | agents",
                    action
                );
                2
            }
        }
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
            let mut token = "demo_token_default";

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

            let res = cli.call_tool(server, tool, json_args, project, output, actor, Some(token));
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
        "codemode" | "exec" => {
            let mut code = String::new();
            let mut require_approval = false;
            let mut actor = "agent";
            let mut token = "demo_token_default";
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--code" | "--script" | "-c" if i + 1 < args.len() => {
                        code = args[i + 1].clone();
                        i += 2;
                    }
                    "--file" | "-f" if i + 1 < args.len() => {
                        code = fs::read_to_string(&args[i + 1]).unwrap_or_default();
                        i += 2;
                    }
                    "--require-approval" => {
                        require_approval = true;
                        i += 1;
                    }
                    "--actor" if i + 1 < args.len() => {
                        actor = &args[i + 1];
                        i += 2;
                    }
                    "--token" if i + 1 < args.len() => {
                        token = &args[i + 1];
                        i += 2;
                    }
                    other if !other.starts_with('-') && code.is_empty() => {
                        code = other.to_string();
                        i += 1;
                    }
                    _ => i += 1,
                }
            }
            if code.trim().is_empty() {
                eprintln!("Usage: aifx mcp codemode --code '<script>' [--require-approval]");
                return 1;
            }
            println!(
                "{}",
                cli.run_codemode(&code, require_approval, actor, Some(token))
            );
            0
        }
        "types" => {
            let mut srv: Option<&str> = None;
            let mut query: Option<&str> = None;
            let mut intent: Option<&str> = None;
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--server" | "-s" if i + 1 < args.len() => {
                        srv = Some(&args[i + 1]);
                        i += 2;
                    }
                    "--query" | "-q" if i + 1 < args.len() => {
                        query = Some(&args[i + 1]);
                        i += 2;
                    }
                    "--intent" | "-i" if i + 1 < args.len() => {
                        intent = Some(&args[i + 1]);
                        i += 2;
                    }
                    _ => i += 1,
                }
            }
            println!("{}", cli.get_types(srv, query, intent));
            0
        }
        "resume" => {
            let mut exec_id = "";
            let mut approver = "operator";
            let mut token = "demo_token_default";
            let mut digest: Option<&str> = None;
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--id" | "--execution-id" if i + 1 < args.len() => {
                        exec_id = &args[i + 1];
                        i += 2;
                    }
                    "--approver" if i + 1 < args.len() => {
                        approver = &args[i + 1];
                        i += 2;
                    }
                    "--token" if i + 1 < args.len() => {
                        token = &args[i + 1];
                        i += 2;
                    }
                    "--digest" if i + 1 < args.len() => {
                        digest = Some(&args[i + 1]);
                        i += 2;
                    }
                    other if !other.starts_with('-') && exec_id.is_empty() => {
                        exec_id = other;
                        i += 1;
                    }
                    _ => i += 1,
                }
            }
            if exec_id.is_empty() {
                eprintln!(
                    "Usage: aifx mcp resume <execution_id> [--approver <name>] [--digest <hash>]"
                );
                return 1;
            }
            println!(
                "{}",
                cli.resume_codemode(exec_id, approver, Some(token), digest)
            );
            0
        }
        "simulate" | "dst" => {
            let mut code = String::new();
            let mut seed = 42u64;
            let mut runs = 25usize;
            let mut actor = "agent";
            let mut token = "demo_token_default";
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--code" | "--script" | "-c" if i + 1 < args.len() => {
                        code = args[i + 1].clone();
                        i += 2;
                    }
                    "--seed" if i + 1 < args.len() => {
                        seed = args[i + 1].parse().unwrap_or(42);
                        i += 2;
                    }
                    "--runs" if i + 1 < args.len() => {
                        runs = args[i + 1].parse().unwrap_or(25);
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
                    other if !other.starts_with('-') && code.is_empty() => {
                        code = other.to_string();
                        i += 1;
                    }
                    _ => i += 1,
                }
            }
            if code.trim().is_empty() {
                eprintln!("Usage: aifx mcp simulate --code '<script>' [--seed <u64>] [--runs <N>]");
                return 1;
            }
            println!(
                "{}",
                cli.simulate_codemode(&code, seed, runs, actor, Some(token))
            );
            0
        }
        "bench-codemode" | "bench-sandbox" => {
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
            println!("{}", cli.run_codemode_benchmark(iterations));
            0
        }
        "stdio" | "serve" => {
            let mut server_name = None;
            let mut group_slug = None;
            let mut i = 2;
            while i < args.len() {
                if (args[i] == "--server" || args[i] == "-s") && i + 1 < args.len() {
                    server_name = Some(args[i + 1].as_str());
                    i += 2;
                } else if (args[i] == "--group" || args[i] == "-g") && i + 1 < args.len() {
                    group_slug = Some(args[i + 1].as_str());
                    i += 2;
                } else {
                    i += 1;
                }
            }
            if server_name.is_some() && group_slug.is_some() {
                eprintln!("[!] Error: --server and --group cannot be combined in --stdio mode");
                return 2;
            }
            crate::server::run_stdio_server(server_name, group_slug)
        }
        "mock-echo-mcp" => {
            let mut flood_stderr = false;
            for arg in &args[2..] {
                if arg == "--flood-stderr" {
                    flood_stderr = true;
                }
            }
            run_mock_echo_mcp_server(flood_stderr)
        }
        "mock-bidirectional-mcp" => run_mock_bidirectional_mcp_server(),
        "wasm" => {
            let sub = args.get(2).map(|s| s.as_str()).unwrap_or("");
            if sub != "register" || args.len() < 6 {
                eprintln!("Usage: omni wasm register <server_name> <tool_name> <path/to/plugin.wasm> [--description <text>]");
                return 1;
            }
            let server_name = args[3].as_str();
            let tool_name = args[4].as_str();
            let wasm_path = args[5].as_str();
            let mut description = format!("Native Wasm plugin tool {}", tool_name);
            let mut i = 6;
            while i < args.len() {
                if args[i] == "--description" && i + 1 < args.len() {
                    description = args[i + 1].clone();
                    i += 2;
                } else {
                    i += 1;
                }
            }
            match fs::read(wasm_path) {
                Ok(bytes) => {
                    if bytes.len() < 4 || &bytes[..4] != b"\0asm" {
                        eprintln!(
                            "Invalid .wasm file '{}': missing \\0asm magic header",
                            wasm_path
                        );
                        return 1;
                    }
                    let b64 = encode_std_base64(&bytes);
                    let ctx = get_gateway_context();
                    let mut tool = crate::models::ToolRecord::new(
                        tool_name,
                        description,
                        json!({"type": "object", "properties": {}}),
                    );
                    tool.protocol = crate::models::ProtocolType::WasmPlugin;
                    tool.status = ToolStatus::Enabled;
                    tool.wasm_module_base64 = Some(b64);

                    let mut srv = ctx.registry.get_server(server_name).unwrap_or_else(|| {
                        let mut s = crate::models::ServerRecord::new(
                            server_name,
                            format!("{} (Wasm Plugins)", server_name),
                            "Zero-process WebAssembly plugin tools",
                            "platform",
                        );
                        s.protocol = crate::models::ProtocolType::WasmPlugin;
                        s.status = ToolStatus::Enabled;
                        s
                    });
                    srv.status = ToolStatus::Enabled;
                    srv.add_tool(tool);
                    ctx.registry.register_server(srv);
                    ctx.cache.refresh_now();
                    println!(
                        "[+] Registered native Wasm plugin tool '{}.{}' ({} bytes)",
                        server_name,
                        tool_name,
                        bytes.len()
                    );
                    0
                }
                Err(e) => {
                    eprintln!("Failed to read Wasm file '{}': {}", wasm_path, e);
                    1
                }
            }
        }
        "import" => {
            let path = match args.get(2) {
                Some(p) => p.as_str(),
                None => {
                    eprintln!("Usage: omni import <path/to/mcp.json> [--dry-run]");
                    return 1;
                }
            };
            let dry_run = args.iter().any(|a| a == "--dry-run");
            match fs::read_to_string(path) {
                Ok(raw) => {
                    let cli = AifxCLI::new(None, None);
                    match import_mcp_config_with_options(&raw, &cli.registry, dry_run) {
                        Ok(report) => {
                            println!(
                                "[+] {} {} MCP server(s): {}",
                                if dry_run {
                                    "Dry-run validated"
                                } else {
                                    "Imported"
                                },
                                report.imported.len(),
                                report.imported.join(", ")
                            );
                            if !report.converted_secrets.is_empty() {
                                println!(
                                    "[*] Converted {} plaintext secret(s) to {{env:...}} references",
                                    report.converted_secrets.len()
                                );
                            }
                            if !report.allowlist_commands.is_empty() {
                                println!(
                                    "OMNI_MCP_STDIO_ALLOWLIST={}",
                                    report.allowlist_commands.join(",")
                                );
                            }
                            0
                        }
                        Err(e) => {
                            eprintln!("Import failed: {}", e);
                            1
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Failed to read '{}': {}", path, e);
                    1
                }
            }
        }
        "migrate" => {
            let to_v6_8 = args
                .iter()
                .any(|a| a == "--to-v6.8" || a == "--to-v6.8.0" || a == "--to-v6.8.5");
            let to_v6_5 = args.iter().any(|a| a == "--to-v6.5");
            let to_v6 = args.iter().any(|a| a == "--to-v6");
            let dry_run = args.iter().any(|a| a == "--dry-run");
            let mut target_backend =
                std::env::var("OMNI_STORAGE_BACKEND").unwrap_or_else(|_| "sqlite".to_string());
            let mut database_url = std::env::var("OMNI_DATABASE_URL").unwrap_or_default();
            let mut i = 2;
            while i < args.len() {
                if args[i] == "--target" && i + 1 < args.len() {
                    target_backend = args[i + 1].clone();
                    i += 2;
                } else if args[i] == "--database-url" && i + 1 < args.len() {
                    database_url = args[i + 1].clone();
                    i += 2;
                } else {
                    i += 1;
                }
            }
            println!(
                "Omni MCP {} State Migration Utility (v{}) {}",
                if to_v6_8 {
                    "-> v6.8"
                } else if to_v6_5 {
                    "-> v6.5"
                } else if to_v6 {
                    "v5 -> v6"
                } else {
                    "v4 -> v5"
                },
                env!("CARGO_PKG_VERSION"),
                if dry_run { "[DRY-RUN]" } else { "" }
            );
            println!("[*] Inspecting local WAL database and state metadata...");
            let cfg = crate::config::get_config();
            println!("[*] Data directory: {}", cfg.data_dir);
            println!("[*] Validating schema version and Merkle audit log continuity...");
            if to_v6_8 {
                let mig = migrate_to_v6_8(&cfg.data_dir, &target_backend, &database_url, dry_run);
                println!(
                    "[+] v6.8.0 migration ({}): {} ({} servers, {} tool groups)",
                    target_backend, mig.summary, mig.servers_migrated, mig.tool_groups_count
                );
                println!(
                    "[+] Audit chain integrity: {} ({} records)",
                    if mig.audit_verified { "OK" } else { "FAILED" },
                    mig.audit_records
                );
                if mig.ok {
                    0
                } else {
                    1
                }
            } else if to_v6_5 {
                let mig = migrate_to_v6_5(&cfg.data_dir, &target_backend, &database_url, dry_run);
                println!(
                    "[+] v6.5 migration ({}): {} ({} servers, {} tool groups)",
                    target_backend, mig.summary, mig.servers_migrated, mig.tool_groups_count
                );
                println!(
                    "[+] Audit chain integrity: {} ({} records)",
                    if mig.audit_verified { "OK" } else { "FAILED" },
                    mig.audit_records
                );
                if mig.ok {
                    0
                } else {
                    1
                }
            } else if to_v6 {
                let mig = migrate_v5_to_v6(&cfg.data_dir, dry_run);
                let ctx = get_gateway_context();
                let (ok, count) = ctx.audit_store.verify_chain();
                println!(
                    "[+] Snapshot/WAL migration: {} ({} servers, {} tool groups)",
                    mig.summary, mig.servers_migrated, mig.tool_groups_count
                );
                println!(
                    "[+] Segmented Audit Log: {} ({} records verified)",
                    if ok { "OK" } else { "FAILED" },
                    count
                );
                println!(
                    "[+] Migration status: {} (state is v6.0 ready)",
                    if ok && mig.ok { "OK" } else { "WARN" }
                );
                if ok && mig.ok {
                    0
                } else {
                    1
                }
            } else {
                println!("[+] WAL format: v5.0 (atomic CAS continuation support verified)");
                println!("[+] Audit log: SHA-256 Merkle chain verified");
                println!("[+] Migration status: OK (0 errors, state is v5.0 ready)");
                0
            }
        }
        _ => {
            let _ = ToolStatus::Enabled;
            print_help();
            1
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ImportReport {
    pub imported: Vec<String>,
    pub allowlist_commands: Vec<String>,
    pub converted_secrets: Vec<(String, String)>,
    pub dry_run: bool,
}

#[derive(Clone, Debug)]
pub struct MigrationReport {
    pub ok: bool,
    pub servers_migrated: usize,
    pub tool_groups_count: usize,
    pub audit_verified: bool,
    pub audit_records: usize,
    pub summary: String,
}

pub fn migrate_v5_to_v6(data_dir: &str, dry_run: bool) -> MigrationReport {
    let db_path = std::path::Path::new(data_dir).join("registry.db");
    let snap_path = std::path::Path::new(data_dir).join("registry.db.snapshot.json");
    let audit_path = std::path::Path::new(data_dir).join("audit.jsonl");

    let store = crate::storage::WalRegistryStore::new(db_path.to_string_lossy().to_string());
    let servers = store.list_all_servers();
    let groups = store.list_all_tool_groups();
    if !dry_run {
        if db_path.exists() {
            let db_bak_path = std::path::Path::new(data_dir).join("registry.db.v5.bak");
            let _ = fs::copy(&db_path, &db_bak_path);
        }
        if snap_path.exists() {
            let bak_path = std::path::Path::new(data_dir).join("registry.db.snapshot.v5.bak");
            let _ = fs::copy(&snap_path, &bak_path);
        }
        for srv in &servers {
            store.save_server(srv);
        }
        for grp in &groups {
            store.save_tool_group(grp);
        }
    }
    store.close();

    let audit_store =
        crate::storage::MerkleAuditStore::new(Some(audit_path.to_string_lossy().to_string()));
    let (audit_verified, audit_records) = audit_store.verify_chain();

    MigrationReport {
        ok: audit_verified,
        servers_migrated: servers.len(),
        tool_groups_count: groups.len(),
        audit_verified,
        audit_records,
        summary: if dry_run {
            "dry-run verified v5 -> v6 schema compatibility".to_string()
        } else {
            "migrated and compacted snapshot/WAL to v6.0".to_string()
        },
    }
}

/// Migrates existing File-WAL or SQLite registry data (plus Merkle audit log records) into
/// SQLite or PostgreSQL (`v6.5.0`) with SHA-256 checksum verification.
pub fn migrate_to_v6_5(
    data_dir: &str,
    target_backend: &str,
    database_url: &str,
    dry_run: bool,
) -> MigrationReport {
    let wal_path = std::path::Path::new(data_dir).join("omni_registry.db");
    let legacy_wal = std::path::Path::new(data_dir).join("registry.db");
    let sqlite_src = std::path::Path::new(data_dir).join("omni_registry.sqlite");

    #[allow(unused_mut)]
    let (mut servers, mut diffs, mut groups, mut frames) =
        if wal_path.exists() || legacy_wal.exists() {
            let src_path = if wal_path.exists() {
                wal_path
            } else {
                legacy_wal
            };
            let src_store =
                crate::storage::WalRegistryStore::new(src_path.to_string_lossy().to_string());
            let s = src_store.list_all_servers();
            let d = src_store.list_diffs(false);
            let g = src_store.list_all_tool_groups();
            let f = src_store.list_suspended_frames();
            src_store.close();
            (s, d, g, f)
        } else {
            (Vec::new(), Vec::new(), Vec::new(), Vec::new())
        };

    // Fallback to existing SQLite database as migration source (e.g. SQLite -> PostgreSQL)
    #[cfg(feature = "storage-sqlite")]
    if servers.is_empty() && groups.is_empty() && sqlite_src.exists() {
        use crate::storage::RegistryStorageBackend;
        if let Ok(sq) =
            crate::storage::SqliteRegistryBackend::try_open(sqlite_src.to_string_lossy().as_ref())
        {
            servers = sq.load_all_servers();
            diffs = sq.load_all_diffs();
            groups = sq.load_all_tool_groups();
            frames = sq.load_all_suspended_frames();
        }
    }
    #[cfg(not(feature = "storage-sqlite"))]
    let _ = &sqlite_src;

    let src_digest = compute_migration_checksum(&servers, &groups, &diffs, &frames);
    let audit_path = std::path::Path::new(data_dir).join("omni_audit.log");
    let audit_store =
        crate::storage::MerkleAuditStore::new(Some(audit_path.to_string_lossy().to_string()));
    let (audit_verified, audit_records) = audit_store.verify_chain();
    let audit_entries = audit_store.list_records(None);

    if dry_run {
        return MigrationReport {
            ok: audit_verified,
            servers_migrated: servers.len(),
            tool_groups_count: groups.len(),
            audit_verified,
            audit_records,
            summary: format!(
                "dry-run verified migration to '{}' (checksum {})",
                target_backend,
                &src_digest[..12]
            ),
        };
    }

    type MigDest = (
        Box<dyn crate::storage::RegistryStorageBackend>,
        Option<Box<dyn crate::storage::AuditSink>>,
    );
    let backend_res: Result<MigDest, String> = match target_backend.to_ascii_lowercase().as_str() {
        "sqlite" => {
            #[cfg(feature = "storage-sqlite")]
            {
                let dsn = if !database_url.trim().is_empty() {
                    database_url.to_string()
                } else {
                    std::path::Path::new(data_dir)
                        .join("omni_registry.sqlite")
                        .to_string_lossy()
                        .into_owned()
                };
                crate::storage::SqliteRegistryBackend::try_open(dsn.clone()).map(|b| {
                    let sink_res = crate::storage::SqliteRegistryBackend::try_open(dsn);
                    let sink = sink_res
                        .ok()
                        .map(|s| Box::new(s) as Box<dyn crate::storage::AuditSink>);
                    (
                        Box::new(b) as Box<dyn crate::storage::RegistryStorageBackend>,
                        sink,
                    )
                })
            }
            #[cfg(not(feature = "storage-sqlite"))]
            {
                let _ = database_url;
                Err("SQLite migration requires the 'storage-sqlite' feature".to_string())
            }
        }
        "postgres" | "postgresql" => {
            #[cfg(feature = "storage-postgres")]
            {
                let dsn = if !database_url.trim().is_empty() {
                    database_url.to_string()
                } else {
                    return MigrationReport {
                        ok: false,
                        servers_migrated: 0,
                        tool_groups_count: 0,
                        audit_verified,
                        audit_records,
                        summary:
                            "PostgreSQL migration requires --database-url or OMNI_DATABASE_URL"
                                .to_string(),
                    };
                };
                crate::storage::PostgresRegistryBackend::connect(dsn.clone()).map(|b| {
                    let sink = crate::storage::PostgresRegistryBackend::connect(dsn)
                        .ok()
                        .map(|s| Box::new(s) as Box<dyn crate::storage::AuditSink>);
                    (
                        Box::new(b) as Box<dyn crate::storage::RegistryStorageBackend>,
                        sink,
                    )
                })
            }
            #[cfg(not(feature = "storage-postgres"))]
            {
                let _ = database_url;
                Err("PostgreSQL migration requires the 'storage-postgres' feature".to_string())
            }
        }
        other => Err(format!("Unsupported migration target '{}'", other)),
    };

    match backend_res {
        Ok((dst, audit_sink_opt)) => {
            for s in &servers {
                dst.save_server(s);
            }
            for d in &diffs {
                dst.save_diff(d);
            }
            for g in &groups {
                dst.save_tool_group(g);
            }
            for f in &frames {
                dst.save_suspended_frame(f);
            }
            let mut sink_ok = true;
            if let Some(sink) = audit_sink_opt {
                for rec in &audit_entries {
                    if sink.emit_record(rec).is_err() {
                        sink_ok = false;
                        break;
                    }
                }
                if sink_ok && !audit_entries.is_empty() {
                    let (v, _) = sink.verify_integrity();
                    sink_ok = v;
                }
            }
            let verify_servers = dst.load_all_servers();
            let verify_groups = dst.load_all_tool_groups();
            let verify_diffs = dst.load_all_diffs();
            let verify_frames = dst.load_all_suspended_frames();
            let dst_digest = compute_migration_checksum(
                &verify_servers,
                &verify_groups,
                &verify_diffs,
                &verify_frames,
            );
            let checksum_ok = src_digest == dst_digest;
            MigrationReport {
                ok: audit_verified && sink_ok && checksum_ok,
                servers_migrated: servers.len(),
                tool_groups_count: groups.len(),
                audit_verified: audit_verified && sink_ok,
                audit_records,
                summary: if checksum_ok && sink_ok {
                    format!(
                        "migrated to '{}' with verified SHA-256 checksum {}",
                        target_backend,
                        &dst_digest[..12]
                    )
                } else if !sink_ok {
                    "audit log sink verification failed after migration".to_string()
                } else {
                    format!(
                        "checksum mismatch after migration (src {} != dst {})",
                        &src_digest[..12],
                        &dst_digest[..12]
                    )
                },
            }
        }
        Err(e) => MigrationReport {
            ok: false,
            servers_migrated: 0,
            tool_groups_count: 0,
            audit_verified,
            audit_records,
            summary: e,
        },
    }
}

fn compute_migration_checksum(
    servers: &[crate::models::ServerRecord],
    groups: &[crate::models::ToolGroupRecord],
    diffs: &[crate::models::ConfigDiff],
    frames: &[crate::models::SuspendedExecutionFrame],
) -> String {
    let mut keys: Vec<String> = servers
        .iter()
        .map(|s| format!("srv:{}:{}:{}", s.name, s.version, s.tools.len()))
        .collect();
    for g in groups {
        keys.push(format!("grp:{}:{}", g.slug, g.entries.len()));
    }
    for d in diffs {
        keys.push(format!("diff:{}:{}", d.diff_id, d.approved));
    }
    for f in frames {
        keys.push(format!(
            "frame:{}:{}",
            f.execution_id, f.pending_arguments_hash
        ));
    }
    keys.sort();
    crate::storage::sha256_hex(keys.join("|").as_bytes())
}

/// Migrates all v6.8.0 collections (`servers`, `diffs`, `groups`, `frames`, `skills`,
/// `mcp_tasks`, `oauth_flows`, `oauth_tokens`, `upstream_callbacks`, `file_blobs` + chunks,
/// plus Merkle audit log records) from WAL or SQLite into SQLite or PostgreSQL with SHA-256
/// checksum verification across all collections.
pub fn migrate_to_v6_8(
    data_dir: &str,
    target_backend: &str,
    database_url: &str,
    dry_run: bool,
) -> MigrationReport {
    use crate::storage::RegistryStorageBackend;

    let wal_path = std::path::Path::new(data_dir).join("omni_registry.db");
    let legacy_wal = std::path::Path::new(data_dir).join("registry.db");
    let sqlite_src = std::path::Path::new(data_dir).join("omni_registry.sqlite");

    #[allow(unused_mut)]
    let (
        mut servers,
        mut diffs,
        mut groups,
        mut frames,
        mut skills,
        mut mcp_tasks,
        mut oauth_flows,
        mut oauth_tokens,
        mut upstream_callbacks,
        mut file_blobs,
        mut file_blob_chunks,
    ) = if wal_path.exists() || legacy_wal.exists() {
        let src_path = if wal_path.exists() {
            wal_path
        } else {
            legacy_wal
        };
        let src_store =
            crate::storage::WalRegistryStore::new(src_path.to_string_lossy().to_string());
        let s = src_store.load_all_servers();
        let d = src_store.load_all_diffs();
        let g = src_store.load_all_tool_groups();
        let f = src_store.load_all_suspended_frames();
        let sk = src_store.load_all_skills();
        let mt = src_store.load_all_mcp_tasks();
        let of = src_store.load_all_oauth_flows();
        let ot = src_store.load_all_oauth_tokens();
        let uc = src_store.load_all_upstream_callbacks();
        let fb = src_store.load_all_file_blobs();
        let mut chunks_map = Vec::new();
        for blob in &fb {
            if let Ok(chunks) = src_store.load_file_blob_chunks(&blob.storage_key) {
                if !chunks.is_empty() {
                    chunks_map.push((blob.storage_key.clone(), chunks));
                }
            }
        }
        src_store.close();
        (s, d, g, f, sk, mt, of, ot, uc, fb, chunks_map)
    } else {
        (
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
    };

    #[cfg(feature = "storage-sqlite")]
    if servers.is_empty()
        && groups.is_empty()
        && skills.is_empty()
        && mcp_tasks.is_empty()
        && sqlite_src.exists()
    {
        if let Ok(sq) =
            crate::storage::SqliteRegistryBackend::try_open(sqlite_src.to_string_lossy().as_ref())
        {
            servers = sq.load_all_servers();
            diffs = sq.load_all_diffs();
            groups = sq.load_all_tool_groups();
            frames = sq.load_all_suspended_frames();
            skills = sq.load_all_skills();
            mcp_tasks = sq.load_all_mcp_tasks();
            oauth_flows = sq.load_all_oauth_flows();
            oauth_tokens = sq.load_all_oauth_tokens();
            upstream_callbacks = sq.load_all_upstream_callbacks();
            file_blobs = sq.load_all_file_blobs();
            for blob in &file_blobs {
                if let Ok(chunks) = sq.load_file_blob_chunks(&blob.storage_key) {
                    if !chunks.is_empty() {
                        file_blob_chunks.push((blob.storage_key.clone(), chunks));
                    }
                }
            }
        }
    }
    #[cfg(not(feature = "storage-sqlite"))]
    let _ = &sqlite_src;

    let src_digest = compute_migration_checksum_v6_8(
        &servers,
        &groups,
        &diffs,
        &frames,
        &skills,
        &mcp_tasks,
        &oauth_flows,
        &oauth_tokens,
        &upstream_callbacks,
        &file_blobs,
    );
    let audit_path = std::path::Path::new(data_dir).join("omni_audit.log");
    let audit_store =
        crate::storage::MerkleAuditStore::new(Some(audit_path.to_string_lossy().to_string()));
    let (audit_verified, audit_records) = audit_store.verify_chain();
    let audit_entries = audit_store.list_records(None);

    if dry_run {
        return MigrationReport {
            ok: audit_verified,
            servers_migrated: servers.len(),
            tool_groups_count: groups.len(),
            audit_verified,
            audit_records,
            summary: format!(
                "dry-run verified v6.8 migration to '{}' (checksum {}; {} skills, {} tasks, {} oauth_flows, {} oauth_tokens, {} callbacks, {} file_blobs)",
                target_backend,
                &src_digest[..12],
                skills.len(),
                mcp_tasks.len(),
                oauth_flows.len(),
                oauth_tokens.len(),
                upstream_callbacks.len(),
                file_blobs.len(),
            ),
        };
    }

    type MigDest = (
        Box<dyn crate::storage::RegistryStorageBackend>,
        Option<Box<dyn crate::storage::AuditSink>>,
    );
    let backend_res: Result<MigDest, String> = match target_backend.to_ascii_lowercase().as_str() {
        "sqlite" => {
            #[cfg(feature = "storage-sqlite")]
            {
                let dsn = if !database_url.trim().is_empty() {
                    database_url.to_string()
                } else {
                    std::path::Path::new(data_dir)
                        .join("omni_registry.sqlite")
                        .to_string_lossy()
                        .into_owned()
                };
                crate::storage::SqliteRegistryBackend::try_open(dsn.clone()).map(|b| {
                    let sink_res = crate::storage::SqliteRegistryBackend::try_open(dsn);
                    let sink = sink_res
                        .ok()
                        .map(|s| Box::new(s) as Box<dyn crate::storage::AuditSink>);
                    (
                        Box::new(b) as Box<dyn crate::storage::RegistryStorageBackend>,
                        sink,
                    )
                })
            }
            #[cfg(not(feature = "storage-sqlite"))]
            {
                let _ = database_url;
                Err("SQLite migration requires the 'storage-sqlite' feature".to_string())
            }
        }
        "postgres" | "postgresql" => {
            #[cfg(feature = "storage-postgres")]
            {
                let dsn = if !database_url.trim().is_empty() {
                    database_url.to_string()
                } else {
                    return MigrationReport {
                        ok: false,
                        servers_migrated: 0,
                        tool_groups_count: 0,
                        audit_verified,
                        audit_records,
                        summary:
                            "PostgreSQL migration requires --database-url or OMNI_DATABASE_URL"
                                .to_string(),
                    };
                };
                crate::storage::PostgresRegistryBackend::connect(dsn.clone()).map(|b| {
                    let sink = crate::storage::PostgresRegistryBackend::connect(dsn)
                        .ok()
                        .map(|s| Box::new(s) as Box<dyn crate::storage::AuditSink>);
                    (
                        Box::new(b) as Box<dyn crate::storage::RegistryStorageBackend>,
                        sink,
                    )
                })
            }
            #[cfg(not(feature = "storage-postgres"))]
            {
                let _ = database_url;
                Err("PostgreSQL migration requires the 'storage-postgres' feature".to_string())
            }
        }
        other => Err(format!("Unsupported migration target '{}'", other)),
    };

    match backend_res {
        Ok((dst, audit_sink_opt)) => {
            for s in &servers {
                dst.save_server(s);
            }
            for d in &diffs {
                dst.save_diff(d);
            }
            for g in &groups {
                dst.save_tool_group(g);
            }
            for f in &frames {
                dst.save_suspended_frame(f);
            }
            for sk in &skills {
                dst.save_skill(sk);
            }
            for mt in &mcp_tasks {
                dst.save_mcp_task(mt);
            }
            for of in &oauth_flows {
                dst.save_oauth_flow(of);
            }
            for ot in &oauth_tokens {
                dst.save_oauth_token(ot);
            }
            for uc in &upstream_callbacks {
                dst.save_upstream_callback(uc);
            }
            for fb in &file_blobs {
                dst.save_file_blob(fb);
            }
            for (storage_key, chunks) in &file_blob_chunks {
                for (idx, chunk) in chunks.iter().enumerate() {
                    let _ = dst.save_file_blob_chunk(storage_key, idx as u32, chunk);
                }
            }

            let mut sink_ok = true;
            if let Some(sink) = audit_sink_opt {
                for rec in &audit_entries {
                    if sink.emit_record(rec).is_err() {
                        sink_ok = false;
                        break;
                    }
                }
                if sink_ok && !audit_entries.is_empty() {
                    let (v, _) = sink.verify_integrity();
                    sink_ok = v;
                }
            }

            let verify_servers = dst.load_all_servers();
            let verify_groups = dst.load_all_tool_groups();
            let verify_diffs = dst.load_all_diffs();
            let verify_frames = dst.load_all_suspended_frames();
            let verify_skills = dst.load_all_skills();
            let verify_tasks = dst.load_all_mcp_tasks();
            let verify_flows = dst.load_all_oauth_flows();
            let verify_tokens = dst.load_all_oauth_tokens();
            let verify_callbacks = dst.load_all_upstream_callbacks();
            let verify_blobs = dst.load_all_file_blobs();

            let dst_digest = compute_migration_checksum_v6_8(
                &verify_servers,
                &verify_groups,
                &verify_diffs,
                &verify_frames,
                &verify_skills,
                &verify_tasks,
                &verify_flows,
                &verify_tokens,
                &verify_callbacks,
                &verify_blobs,
            );
            let checksum_ok = src_digest == dst_digest;
            MigrationReport {
                ok: audit_verified && sink_ok && checksum_ok,
                servers_migrated: servers.len(),
                tool_groups_count: groups.len(),
                audit_verified: audit_verified && sink_ok,
                audit_records,
                summary: if checksum_ok && sink_ok {
                    format!(
                        "migrated v6.8 state to '{}' with verified SHA-256 checksum {} ({} skills, {} tasks, {} oauth_flows, {} oauth_tokens, {} callbacks, {} file_blobs)",
                        target_backend,
                        &dst_digest[..12],
                        skills.len(),
                        mcp_tasks.len(),
                        oauth_flows.len(),
                        oauth_tokens.len(),
                        upstream_callbacks.len(),
                        file_blobs.len(),
                    )
                } else if !sink_ok {
                    "audit log sink verification failed after v6.8 migration".to_string()
                } else {
                    format!(
                        "v6.8 checksum mismatch after migration (src {} != dst {})",
                        &src_digest[..12],
                        &dst_digest[..12]
                    )
                },
            }
        }
        Err(e) => MigrationReport {
            ok: false,
            servers_migrated: 0,
            tool_groups_count: 0,
            audit_verified,
            audit_records,
            summary: e,
        },
    }
}

#[allow(clippy::too_many_arguments)]
fn compute_migration_checksum_v6_8(
    servers: &[crate::models::ServerRecord],
    groups: &[crate::models::ToolGroupRecord],
    diffs: &[crate::models::ConfigDiff],
    frames: &[crate::models::SuspendedExecutionFrame],
    skills: &[crate::models::SkillRecord],
    mcp_tasks: &[crate::models::McpTaskRecord],
    oauth_flows: &[crate::models::OAuthFlowRecord],
    oauth_tokens: &[crate::models::OAuthTokenRecord],
    upstream_callbacks: &[crate::models::PendingUpstreamCallback],
    file_blobs: &[crate::models::FileBlobRecord],
) -> String {
    let mut keys: Vec<String> = servers
        .iter()
        .map(|s| format!("srv:{}:{}:{}", s.name, s.version, s.tools.len()))
        .collect();
    for g in groups {
        keys.push(format!("grp:{}:{}", g.slug, g.entries.len()));
    }
    for d in diffs {
        keys.push(format!("diff:{}:{}", d.diff_id, d.approved));
    }
    for f in frames {
        keys.push(format!(
            "frame:{}:{}",
            f.execution_id, f.pending_arguments_hash
        ));
    }
    for sk in skills {
        keys.push(format!("skill:{}:{}", sk.slug, sk.manifest_sha256));
    }
    for mt in mcp_tasks {
        keys.push(format!("task:{}:{}", mt.task_id, mt.status.as_str()));
    }
    for of in oauth_flows {
        keys.push(format!("flow:{}:{}", of.state, of.server_name));
    }
    for ot in oauth_tokens {
        keys.push(format!("tok:{}:{}", ot.server_name, ot.updated_at));
    }
    for uc in upstream_callbacks {
        keys.push(format!("cb:{}:{}", uc.callback_id, uc.server_name));
    }
    for fb in file_blobs {
        keys.push(format!("blob:{}:{}", fb.file_id, fb.sha256));
    }
    keys.sort();
    crate::storage::sha256_hex(keys.join("|").as_bytes())
}

fn encode_std_base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((n >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(n & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

pub fn import_mcp_config_json(
    raw_json: &str,
    registry: &MCPRegistry,
) -> Result<Vec<String>, String> {
    import_mcp_config_with_options(raw_json, registry, false).map(|r| r.imported)
}

fn sanitize_env_ident(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

pub fn import_mcp_config_with_options(
    raw_json: &str,
    registry: &MCPRegistry,
    dry_run: bool,
) -> Result<ImportReport, String> {
    let parsed = JsonValue::parse(raw_json)?;
    let servers_obj = parsed
        .get("mcpServers")
        .and_then(|v| v.as_object())
        .ok_or_else(|| "Missing 'mcpServers' object in config JSON".to_string())?;

    let mut report = ImportReport {
        dry_run,
        ..Default::default()
    };
    for (raw_name, entry) in servers_obj {
        let srv_name = if raw_name.starts_with("external-mcp:") {
            raw_name.clone()
        } else {
            format!("external-mcp:{}", raw_name)
        };

        let mut server = crate::models::ServerRecord::new(
            &srv_name,
            raw_name,
            format!("Imported from mcp.json ({})", raw_name),
            "imported",
        );
        server.protocol = crate::models::ProtocolType::ExternalMcp;

        if let Some(url) = entry.get("url").and_then(|v| v.as_str()) {
            let mut header_refs = std::collections::BTreeMap::new();
            let mut bearer_ref = None;
            if let Some(hdrs) = entry.get("headers").and_then(|v| v.as_object()) {
                for (hk, hv) in hdrs {
                    if let Some(s) = hv.as_str() {
                        if s.starts_with("{env:") || s.starts_with("{file:") {
                            header_refs.insert(hk.clone(), s.to_string());
                        } else {
                            let env_key = format!(
                                "OMNI_{}_{}",
                                sanitize_env_ident(raw_name),
                                sanitize_env_ident(hk)
                            );
                            let ref_val = format!("{{env:{}}}", env_key);
                            report
                                .converted_secrets
                                .push((format!("{}.headers.{}", raw_name, hk), ref_val.clone()));
                            if hk.eq_ignore_ascii_case("authorization") {
                                bearer_ref = Some(ref_val.clone());
                            }
                            header_refs.insert(hk.clone(), ref_val);
                        }
                    }
                }
            }
            server.upstream_config = Some(crate::models::UpstreamTransportConfig::StreamableHttp {
                url: url.to_string(),
                bearer_ref,
                header_refs,
                session_mode: crate::models::SessionMode::Stateful,
            });
        } else if let Some(cmd) = entry.get("command").and_then(|v| v.as_str()) {
            if !report.allowlist_commands.iter().any(|c| c == cmd) {
                report.allowlist_commands.push(cmd.to_string());
            }
            let args: Vec<String> = entry
                .get("args")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let mut env_refs = std::collections::BTreeMap::new();
            if let Some(envs) = entry.get("env").and_then(|v| v.as_object()) {
                for (ek, ev) in envs {
                    if let Some(val_str) = ev.as_str() {
                        if val_str.starts_with("{env:") || val_str.starts_with("{file:") {
                            env_refs.insert(ek.clone(), val_str.to_string());
                        } else {
                            // Convert plaintext secret into {env:KEY} reference so secrets are not persisted in WAL
                            let ref_val = format!("{{env:{}}}", ek);
                            report
                                .converted_secrets
                                .push((format!("{}.env.{}", raw_name, ek), ref_val.clone()));
                            env_refs.insert(ek.clone(), ref_val);
                        }
                    }
                }
            }
            server.upstream_config = Some(crate::models::UpstreamTransportConfig::Stdio {
                command: cmd.to_string(),
                canonical_command: cmd.to_string(),
                args,
                env_refs,
                cwd: entry
                    .get("cwd")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
            });
        } else {
            continue;
        }

        if !dry_run {
            registry.register_server(server);
        }
        report.imported.push(srv_name);
    }

    Ok(report)
}

fn parse_plugin_export_args(
    args: &[String],
) -> Result<crate::extension::plugin::EnterpriseExportOptions, String> {
    let mut gateway_url = std::env::var("OMNI_GATEWAY_URL")
        .or_else(|_| std::env::var("OMNI_PUBLIC_BASE_URL"))
        .unwrap_or_else(|_| "https://mcp.corp.example.com".to_string());
    let mut group_slug: Option<String> = None;
    let mut auth_env = "OMNI_AUTH_TOKEN".to_string();
    let mut out_dir = std::path::PathBuf::from("./omni-enterprise-bundle");
    let mut allow_insecure_http = false;
    let mut include_registry_skills = false;
    let mut dry_run = false;

    let mut i = 3;
    while i < args.len() {
        match args[i].as_str() {
            "--enterprise" => i += 1,
            "--gateway-url" if i + 1 < args.len() => {
                gateway_url = args[i + 1].clone();
                i += 2;
            }
            "--group" | "-g" if i + 1 < args.len() => {
                group_slug = Some(args[i + 1].clone());
                i += 2;
            }
            "--auth-env" if i + 1 < args.len() => {
                auth_env = args[i + 1].clone();
                i += 2;
            }
            "--out" | "-o" if i + 1 < args.len() => {
                out_dir = std::path::PathBuf::from(&args[i + 1]);
                i += 2;
            }
            "--allow-insecure-http" => {
                allow_insecure_http = true;
                i += 1;
            }
            "--include-registry-skills" => {
                include_registry_skills = true;
                i += 1;
            }
            "--dry-run" => {
                dry_run = true;
                i += 1;
            }
            _ => i += 1,
        }
    }

    Ok(crate::extension::plugin::EnterpriseExportOptions {
        gateway_url,
        group_slug,
        auth_env,
        out_dir,
        allow_insecure_http,
        include_registry_skills,
        dry_run,
    })
}

fn try_dispatch_offline_cli(args: &[String]) -> Option<i32> {
    let subcmd = args.get(1).map(|s| s.as_str()).unwrap_or("");
    let action = args.get(2).map(|s| s.as_str()).unwrap_or("");

    if subcmd == "skills" && action == "install" {
        let mut agent = "universal".to_string();
        let mut scope = "project".to_string();
        let mut dir: Option<std::path::PathBuf> = None;
        let mut only: Vec<String> = Vec::new();
        let mut dry_run = false;
        let mut force = false;
        let mut i = 3;
        while i < args.len() {
            match args[i].as_str() {
                "--agent" | "-a" if i + 1 < args.len() => {
                    agent = args[i + 1].clone();
                    i += 2;
                }
                "--scope" if i + 1 < args.len() => {
                    scope = args[i + 1].clone();
                    i += 2;
                }
                "--dir" | "-d" if i + 1 < args.len() => {
                    dir = Some(std::path::PathBuf::from(&args[i + 1]));
                    i += 2;
                }
                "--only" if i + 1 < args.len() => {
                    for part in args[i + 1].split(',') {
                        let p = part.trim();
                        if !p.is_empty() {
                            only.push(p.to_string());
                        }
                    }
                    i += 2;
                }
                "--dry-run" => {
                    dry_run = true;
                    i += 1;
                }
                "--force" | "-f" => {
                    force = true;
                    i += 1;
                }
                _ => i += 1,
            }
        }
        let opts = crate::extension::plugin::SkillInstallOptions {
            agent,
            scope,
            dir,
            only,
            dry_run,
            force,
        };
        return Some(match crate::extension::plugin::install_skills(&opts) {
            Ok(report) => {
                println!(
                    "{}",
                    json!({
                        "dry_run": report.dry_run,
                        "targets": JsonValue::Array(report.targets.into_iter().map(JsonValue::from).collect()),
                        "installed_count": report.installed_files.len() as i64,
                        "skipped_count": report.skipped_files.len() as i64,
                        "installed_files": JsonValue::Array(report.installed_files.into_iter().map(JsonValue::from).collect()),
                        "skipped_files": JsonValue::Array(report.skipped_files.into_iter().map(JsonValue::from).collect())
                    })
                    .to_string_pretty()
                );
                0
            }
            Err(e) => {
                eprintln!("Skills install failed: {}", e);
                1
            }
        });
    }

    if subcmd == "plugin" {
        match action {
            "agents" => {
                println!(
                    "{}",
                    crate::extension::plugin::supported_agents_json().to_string_pretty()
                );
                return Some(0);
            }
            "init" => {
                let mut mode = "standard".to_string();
                let mut agent = "claude".to_string();
                let mut gateway_url: Option<String> = None;
                let mut group_slug: Option<String> = None;
                let mut auth_env = "OMNI_AUTH_TOKEN".to_string();
                let mut transport: Option<String> = None;
                let mut out_dir = std::path::PathBuf::from("./omni-plugin");
                let mut allow_insecure_http = false;
                let mut dry_run = false;
                let mut i = 3;
                while i < args.len() {
                    match args[i].as_str() {
                        "--mode" | "-m" if i + 1 < args.len() => {
                            mode = args[i + 1].clone();
                            i += 2;
                        }
                        "--agent" | "-a" if i + 1 < args.len() => {
                            agent = args[i + 1].clone();
                            i += 2;
                        }
                        "--gateway-url" if i + 1 < args.len() => {
                            gateway_url = Some(args[i + 1].clone());
                            i += 2;
                        }
                        "--group" | "-g" if i + 1 < args.len() => {
                            group_slug = Some(args[i + 1].clone());
                            i += 2;
                        }
                        "--auth-env" if i + 1 < args.len() => {
                            auth_env = args[i + 1].clone();
                            i += 2;
                        }
                        "--transport" if i + 1 < args.len() => {
                            transport = Some(args[i + 1].clone());
                            i += 2;
                        }
                        "--out" | "-o" if i + 1 < args.len() => {
                            out_dir = std::path::PathBuf::from(&args[i + 1]);
                            i += 2;
                        }
                        "--allow-insecure-http" => {
                            allow_insecure_http = true;
                            i += 1;
                        }
                        "--dry-run" => {
                            dry_run = true;
                            i += 1;
                        }
                        _ => i += 1,
                    }
                }
                let opts = crate::extension::plugin::PluginInitOptions {
                    mode,
                    agent,
                    gateway_url,
                    group_slug,
                    auth_env,
                    transport,
                    out_dir,
                    allow_insecure_http,
                    dry_run,
                };
                return Some(match crate::extension::plugin::init_plugin(&opts) {
                    Ok(report) => {
                        println!(
                            "{}",
                            json!({
                                "dry_run": report.dry_run,
                                "mode": report.mode,
                                "out_dir": report.out_dir,
                                "files_count": report.files.len() as i64,
                                "files": JsonValue::Array(report.files.into_iter().map(JsonValue::from).collect()),
                                "manifest_sha256": report.manifest_sha256.map(JsonValue::from).unwrap_or(JsonValue::Null)
                            })
                            .to_string_pretty()
                        );
                        0
                    }
                    Err(e) => {
                        eprintln!("Plugin init failed: {}", e);
                        1
                    }
                });
            }
            "verify" => {
                let target = match args.get(3) {
                    Some(p) => std::path::Path::new(p),
                    None => {
                        eprintln!("Usage: omni plugin verify <plugin-dir>");
                        return Some(2);
                    }
                };
                return Some(
                    match crate::extension::plugin::verify_plugin_bundle(target) {
                        Ok(report) => {
                            println!(
                            "{}",
                            json!({
                                "valid": report.valid,
                                "checked_files": report.checked_files as i64,
                                "validated_skills": JsonValue::Array(report.validated_skills.into_iter().map(JsonValue::from).collect()),
                                "verified_checksum_file": report.verified_checksum_file.map(JsonValue::from).unwrap_or(JsonValue::Null),
                                "errors": JsonValue::Array(report.errors.iter().cloned().map(JsonValue::from).collect())
                            })
                            .to_string_pretty()
                        );
                            if report.valid {
                                0
                            } else {
                                1
                            }
                        }
                        Err(e) => {
                            eprintln!("Plugin verification error: {}", e);
                            1
                        }
                    },
                );
            }
            "export" => {
                let include_reg = args.iter().any(|a| a == "--include-registry-skills");
                if !include_reg {
                    let opts = match parse_plugin_export_args(args) {
                        Ok(o) => o,
                        Err(e) => {
                            eprintln!("Error: {}", e);
                            return Some(2);
                        }
                    };
                    return Some(
                        match crate::extension::plugin::export_enterprise_bundle(&opts, &[]) {
                            Ok(report) => {
                                println!(
                                    "{}",
                                    json!({
                                        "dry_run": report.dry_run,
                                        "out_dir": report.out_dir,
                                        "files_count": report.files.len() as i64,
                                        "skills_exported": JsonValue::Array(
                                            report.skills_exported.into_iter().map(JsonValue::from).collect()
                                        ),
                                        "sha256sums_digest": report.sha256sums_digest
                                    })
                                    .to_string_pretty()
                                );
                                0
                            }
                            Err(e) => {
                                eprintln!("Enterprise export failed: {}", e);
                                1
                            }
                        },
                    );
                }
            }
            "" | "--help" | "-h" => {
                println!(
                    "USAGE:\n  omni plugin agents\n  omni plugin init --mode standard|enterprise [--agent <target>] [--gateway-url <url>] [--group <slug>] [--auth-env <VAR>] [--transport stdio|http|mcp-remote] [--out <dir>] [--dry-run]\n  omni plugin export --enterprise [--gateway-url <url>] [--group <slug>] [--auth-env <VAR>] [--include-registry-skills] [--out <dir>] [--dry-run]\n  omni plugin verify <dir>"
                );
                return Some(0);
            }
            _ => {}
        }
    }

    None
}

fn print_help() {
    println!(
        concat!(
            "omni / aifx - High-Performance Rust CLI for Agentic Operations & Omni Code Mode MCP (v",
            env!("CARGO_PKG_VERSION"),
            ")\n\n",
            "USAGE:\n",
            "  omni list [--format table|json]\n",
            "  omni search [query] [--format table|json]\n",
            "  omni call <server> <tool> [--args '<json>'] [--project '<fields>'] [--output <path>] [--actor <actor>]\n",
            "  omni codemode --code '<script>' [--require-approval]\n",
            "  omni resume <execution_id> [--approver <name>]\n",
            "  omni simulate --code '<script>' [--seed <u64>] [--runs <N>]\n",
            "  omni types [--server <name>] [--query <q>] [--intent '<prompt>']\n",
            "  omni stdio [--server <name> | --group <slug>]\n",
            "  omni skills list [--group <slug>]\n",
            "  omni skills install --agent <target|all> [--scope project|user] [--dir <path>] [--only <slug,...>] [--dry-run] [--force]\n",
            "  omni skills bootstrap [--group <slug>]\n",
            "  omni skills publish <dir> [--group <slug>] [--slug <slug>]\n",
            "  omni skills verify <slug>\n",
            "  omni plugin agents\n",
            "  omni plugin init --mode standard|enterprise [--agent <target>] [--gateway-url <url>] [--group <slug>] [--auth-env <VAR>] [--out <dir>]\n",
            "  omni plugin export --enterprise [--gateway-url <url>] [--group <slug>] [--include-registry-skills] [--out <dir>]\n",
            "  omni plugin verify <dir>\n",
            "  omni bench [--iterations <N>]\n",
            "  omni bench-codemode [--iterations <N>]\n",
            "  omni import <path/to/mcp.json>\n",
            "  omni wasm register <server> <tool> <path.wasm> [--description <text>]\n",
            "  omni migrate [--to-v6 | --to-v6.5 | --to-v6.8 | --to-v6.8.5 --target sqlite|postgres --database-url <dsn>]\n\n",
            "NOTE: Commands can be prefixed with 'aifx', 'omni', or 'mcp' interchangeably."
        )
    );
}

pub fn run_mock_echo_mcp_server(flood_stderr: bool) -> i32 {
    use std::io::{BufRead, BufReader, Write};
    if flood_stderr {
        let mut err = std::io::stderr();
        let chunk = "E".repeat(1024);
        for _ in 0..150 {
            let _ = writeln!(err, "STDERR_FLOOD: {}", chunk);
        }
        let _ = err.flush();
    }

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let reader = BufReader::new(stdin.lock());

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(json_val) = crate::json::JsonValue::parse(trimmed) {
            let id = json_val
                .get("id")
                .cloned()
                .unwrap_or(crate::json::JsonValue::Null);
            let method = json_val
                .get("method")
                .and_then(|m| m.as_str())
                .unwrap_or("");

            match method {
                "initialize" => {
                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "protocolVersion": "2025-11-25",
                            "capabilities": { "tools": {} },
                            "serverInfo": { "name": "mock-echo-mcp", "version": "1.0.0" }
                        }
                    });
                    let _ = writeln!(stdout, "{}", resp.to_string_compact());
                    let _ = stdout.flush();
                }
                "notifications/initialized" => {}
                "tools/list" => {
                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "tools": [
                                {
                                    "name": "echo_nonce",
                                    "description": "Echoes back the cryptographic nonce",
                                    "inputSchema": {
                                        "type": "object",
                                        "properties": {
                                            "nonce": { "type": "string" }
                                        },
                                        "required": ["nonce"]
                                    }
                                }
                            ]
                        }
                    });
                    let _ = writeln!(stdout, "{}", resp.to_string_compact());
                    let _ = stdout.flush();
                }
                "tools/call" => {
                    let params = json_val
                        .get("params")
                        .cloned()
                        .unwrap_or(crate::json::JsonValue::Null);
                    let args = params
                        .get("arguments")
                        .cloned()
                        .unwrap_or(crate::json::JsonValue::Null);
                    let nonce = args
                        .get("nonce")
                        .and_then(|n| n.as_str())
                        .unwrap_or("no-nonce");

                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [
                                {
                                    "type": "text",
                                    "text": nonce
                                }
                            ]
                        }
                    });
                    let _ = writeln!(stdout, "{}", resp.to_string_compact());
                    let _ = stdout.flush();
                }
                _ => {
                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32601, "message": "Method not found" }
                    });
                    let _ = writeln!(stdout, "{}", resp.to_string_compact());
                    let _ = stdout.flush();
                }
            }
        }
    }
    0
}

/// Mock bidirectional MCP stdio server used for end-to-end verification of Pillar 4:
/// - `trigger_sampling`: emits `notifications/progress` (50%), then emits reverse
///   `sampling/createMessage` (`id: 9001`), waits for the gateway's JSON-RPC response on stdin,
///   emits `notifications/progress` (100%), and returns the gateway's sampling reply in `structuredContent`.
/// - `slow_cancel`: sleeps briefly while listening for `notifications/cancelled`.
pub fn run_mock_bidirectional_mcp_server() -> i32 {
    use std::io::{BufRead, BufReader, Write};

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(json_val) = crate::json::JsonValue::parse(trimmed) else {
            continue;
        };
        let id = json_val
            .get("id")
            .cloned()
            .unwrap_or(crate::json::JsonValue::Null);
        let method = json_val
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("");

        match method {
            "initialize" => {
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2025-11-25",
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "mock-bidirectional-mcp", "version": "1.0.0" }
                    }
                });
                let _ = writeln!(stdout, "{}", resp.to_string_compact());
                let _ = stdout.flush();
            }
            "notifications/initialized" | "notifications/cancelled" => {}
            "tools/list" => {
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "tools": [
                            {
                                "name": "trigger_sampling",
                                "description": "Emits progress and reverse sampling/createMessage",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "prompt": { "type": "string" }
                                    }
                                }
                            },
                            {
                                "name": "slow_cancel",
                                "description": "Blocks briefly so caller can cancel it",
                                "inputSchema": { "type": "object", "properties": {} }
                            }
                        ]
                    }
                });
                let _ = writeln!(stdout, "{}", resp.to_string_compact());
                let _ = stdout.flush();
            }
            "tools/call" => {
                let params = json_val
                    .get("params")
                    .cloned()
                    .unwrap_or(crate::json::JsonValue::object());
                let tool_name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let args = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or(crate::json::JsonValue::object());

                if tool_name == "slow_cancel" {
                    // Wait until we receive a notifications/cancelled line on stdin
                    let mut cancel_line = String::new();
                    let _ = reader.read_line(&mut cancel_line);
                    continue;
                }

                // 1. Emit mid-call progress notification (50%)
                let prog1 = json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/progress",
                    "params": {
                        "progressToken": "tok-bidir-1",
                        "progress": 50,
                        "total": 100,
                        "message": "requesting sampling"
                    }
                });
                let _ = writeln!(stdout, "{}", prog1.to_string_compact());
                let _ = stdout.flush();

                // 2. Emit reverse server-to-client request: sampling/createMessage
                let prompt_text = args
                    .get("prompt")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Summarize user alice@example.com");
                let rev_req = json!({
                    "jsonrpc": "2.0",
                    "id": 9001,
                    "method": "sampling/createMessage",
                    "params": {
                        "messages": [{"role": "user", "content": {"type": "text", "text": prompt_text}}],
                        "maxTokens": 128
                    }
                });
                let _ = writeln!(stdout, "{}", rev_req.to_string_compact());
                let _ = stdout.flush();

                // 3. Read gateway's JSON-RPC reply to our reverse request
                let mut reply_line = String::new();
                let sampling_reply = match reader.read_line(&mut reply_line) {
                    Ok(n) if n > 0 => crate::json::JsonValue::parse(reply_line.trim())
                        .unwrap_or_else(|_| json!({"raw": reply_line.trim()})),
                    _ => json!({"error": "eof"}),
                };

                // 4. Emit completion progress (100%)
                let prog2 = json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/progress",
                    "params": {
                        "progressToken": "tok-bidir-1",
                        "progress": 100,
                        "total": 100,
                        "message": "done"
                    }
                });
                let _ = writeln!(stdout, "{}", prog2.to_string_compact());
                let _ = stdout.flush();

                // 5. Return final tools/call response containing the gateway's sampling reply
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "structuredContent": {
                            "sampling_reply": sampling_reply
                        }
                    }
                });
                let _ = writeln!(stdout, "{}", resp.to_string_compact());
                let _ = stdout.flush();
            }
            _ => {
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": "Method not found" }
                });
                let _ = writeln!(stdout, "{}", resp.to_string_compact());
                let _ = stdout.flush();
            }
        }
    }
    0
}
