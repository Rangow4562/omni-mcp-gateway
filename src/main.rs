//! `omni-gateway` Binary Entrypoint.
//! Starts the multi-threaded HTTP/1.1 & JSON-RPC 2.0 Gateway Server or dispatches CLI subcommands.

use omni_mcp::config::get_config;
use omni_mcp::extension::run_aifx_cli;
use omni_mcp::server::run_server;
use std::env;
use std::process;

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();

    // Support `omni-gateway mcp ...` as well as `omni-gateway [port]`
    if let Some(first) = args.first() {
        if first == "mcp" || first == "--help" || first == "-h" {
            process::exit(run_aifx_cli(&args));
        }
    }

    let cfg = get_config();
    let host = cfg.host;
    let port = args
        .first()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(cfg.port);

    if let Err(err) = run_server(&host, port) {
        eprintln!("[!] Error running Omni MCP Gateway: {}", err);
        process::exit(1);
    }
}

