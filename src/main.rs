//! CLI: `fde-05-mcp-legacy-bridge <op> [file.json]` (reads JSON input from the file or stdin).
//! `fde-05-mcp-legacy-bridge ops` lists the operations.
use std::io::Read;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let op = args.get(1).map(String::as_str).unwrap_or("ops");
    if op == "serve" {
        // MCP over stdio: one JSON-RPC message per line in, one response per line out.
        use std::io::{BufRead, Write};
        let start = std::time::Instant::now();
        let mut server = fde_05_mcp_legacy_bridge::mcp::Server::default();
        let stdout = std::io::stdout();
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            server.clock_ms = start.elapsed().as_millis() as u64;
            if let Some(resp) = server.handle(&line) {
                let mut out = stdout.lock();
                let _ = writeln!(out, "{resp}");
                let _ = out.flush();
            }
        }
        return;
    }
    if op == "ops" || op == "--help" || op == "-h" {
        println!("fde-05-mcp-legacy-bridge: A raw JSON-RPC MCP server that lets agents call legacy SOAP and SAP systems safely, with rate limits.");
        println!("usage: fde-05-mcp-legacy-bridge <op> [input.json]   (input JSON from file or stdin)");
        println!("       fde-05-mcp-legacy-bridge serve   (MCP server on stdio)");
        for o in fde_05_mcp_legacy_bridge::OPS {
            println!("  {o}");
        }
        return;
    }
    let mut raw = String::new();
    match args.get(2) {
        Some(p) => raw = std::fs::read_to_string(p).expect("read input file"),
        None => {
            std::io::stdin().read_to_string(&mut raw).expect("read stdin");
        }
    }
    let input: serde_json::Value = serde_json::from_str(if raw.trim().is_empty() { "null" } else { &raw })
        .expect("input must be JSON");
    let req = serde_json::json!({"op": op, "input": input}).to_string();
    let out: serde_json::Value = serde_json::from_str(&fde_05_mcp_legacy_bridge::dispatch(&req)).unwrap();
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
    if out["ok"] != true {
        std::process::exit(1);
    }
}
