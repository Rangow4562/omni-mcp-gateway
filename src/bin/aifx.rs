//! `aifx` CLI Binary Entrypoint (Uber Code Mode CLI).

use omni_mcp::extension::run_aifx_cli;
use std::env;
use std::process;

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    process::exit(run_aifx_cli(&args));
}

