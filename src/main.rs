#![forbid(unsafe_code)]
//! `omni-gateway` Binary Entrypoint.
//! Starts the multi-threaded HTTP/1.1 & JSON-RPC 2.0 Gateway Server or dispatches CLI subcommands.

use omni_mcp::config::get_config;
use omni_mcp::extension::run_aifx_cli;
use omni_mcp::server::run_server;
use std::env;
use std::process;

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();

    // Support stdio MCP server mode for harness agents and MCP plugins
    if let Some(first) = args.first() {
        if first == "--version" || first == "-V" || first == "version" {
            println!("omni-mcp v{}", env!("CARGO_PKG_VERSION"));
            process::exit(0);
        }
        if first == "--stdio" || first == "stdio" || first == "serve-stdio" {
            let mut server_name = None;
            let mut i = 1;
            while i < args.len() {
                if (args[i] == "--server" || args[i] == "-s") && i + 1 < args.len() {
                    server_name = Some(args[i + 1].as_str());
                    i += 2;
                } else {
                    i += 1;
                }
            }
            process::exit(omni_mcp::server::run_stdio_server(server_name));
        }
        if first == "mcp" || first == "aifx" || first == "--help" || first == "-h" {
            process::exit(run_aifx_cli(&args));
        }
    }

    let cfg = get_config();
    let host = cfg.host.clone();
    let port = args
        .first()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(cfg.port);

    if let Err(err) = cfg.validate_bind(&host) {
        eprintln!("[!] Configuration Error: {}", err);
        process::exit(1);
    }

    if let Err(err) = run_server(&host, port) {
        eprintln!("[!] Error running Omni MCP Gateway: {}", err);
        process::exit(1);
    }
}
