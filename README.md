# fde-05-mcp-legacy-bridge

**Forward Deployed Engineering — Stage 5 of 12: MCP & Legacy System Integration**

A Model Context Protocol server written on raw JSON-RPC 2.0 that lets an agent call a 2004-era SOAP system and SAP, with schema checks, safe XML, and rate limits the old system can survive.

**Live demo:** https://fde-05-mcp-legacy-bridge.braylo777.workers.dev — the Rust core runs in your browser as WebAssembly. No server, no tracking.

## Why this matters

Agents need tools, and in a large company the tools are SOAP services, SAP OData endpoints and systems that fall over at ten requests a second. An SDK hides the protocol; an FDE has to debug it at the wire. This repo implements MCP directly on JSON-RPC, validates every argument before it reaches the legacy system, translates JSON to SOAP and back without injection holes, and throttles calls so the agent cannot take the mainframe down.

## What it does

- MCP on raw JSON-RPC 2.0: initialize, ping, tools/list, tools/call, notifications, batches, error codes -32700 / -32600 / -32601 / -32602 / -32002
- stdio transport: fde-05-mcp-legacy-bridge serve works with any MCP client
- Input-schema validation before any legacy call (required, types, pattern, no extra args)
- SOAP 1.1: envelope builder with escaped values and validated element names; strict XML reader that refuses DOCTYPE (no XXE); Fault handling
- Per-tool token bucket rate limiting, plus retry planning with full-jitter backoff, Retry-After and a deadline
- SAP OData v2 URL builder: typed filters, quote escaping, $top cap, column checks

## What it does not do (yet)

- The SOAP backend is an in-process mock; production needs an HTTP client and the real WSDL
- No headless-browser fallback for systems with no API
- No real SAP RFC / BAPI or Oracle EBS connector; OData URL building only
- No Streamable HTTP transport or OAuth for the MCP server (stdio only)

## Operations

Every operation takes JSON in and gives JSON out. The CLI, the tests and the demo page all use the same entry point.

| Operation | What it does |
|---|---|
| `mcp` | A full session: a call before initialize is refused (-32002), then tools/list, a good SOAP call, a SOAP Fault returned as a tool error, an injection attempt stopped by the schema (-32602), and a list call. |
| `json_to_soap` | Values are XML-escaped, so a < or & in data cannot break the envelope. Try a key like a><b: it is refused. |
| `soap_to_json` | A real-world response with namespaces, an entity and CDATA. Repeated elements become arrays. |
| `retry_plan` | Upstream answers 503, 503, 429 with Retry-After 2 s, then 200. Waits use full-jitter exponential backoff, except where the server says how long to wait. |
| `odata_query` | An agent asks for a customer's open orders. The customer value tries to inject an OR clause; it stays a quoted literal. top is capped at 100. |

## Run it

You need Rust (stable). No Node, no npm.

```bash
git clone https://github.com/braylo777/fde-05-mcp-legacy-bridge
cd fde-05-mcp-legacy-bridge
cargo test
cargo run -- ops
```

Try the first demo example from the command line:

```bash
python3 -c 'import json;print(json.dumps(json.load(open("web/examples.json"))["ops"][0]["input"]))' | cargo run -q -- mcp
```

## Use it from an MCP client

```bash
cargo build --release
# then point your client at the binary, e.g. Claude Code:
claude mcp add legacy-bridge -- $(pwd)/target/release/fde-05-mcp-legacy-bridge serve
```

Or talk to it by hand:

```bash
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"legacy_get_customer","arguments":{"customer_id":"C-1001"}}}' \
  | cargo run -q -- serve
```

## Build and deploy the demo

```bash
./build.sh    # tests, then compiles the core to web/app.wasm
./deploy.sh   # serves web/ as a Cloudflare Workers static-assets site (needs a Cloudflare login)
```

`web/` is a plain static site (`index.html`, `app.js`, `app.wasm`). It has no build step, so any static host works.

## Layout

```
src/lib.rs        operation list and router
src/api.rs        JSON entry point + the 3 WASM exports (alloc, dealloc, run)
src/main.rs       CLI
src/*.rs          one module per capability, each with unit tests
tests/            integration tests (every demo example must pass)
web/              demo page; examples.json drives both the page and this README
```

## Design choices

- **Rust, few dependencies.** Only `serde`, `serde_json`, `sha2` and `hmac`, pinned to exact versions with a committed `Cargo.lock`.
- **Deterministic.** Time, randomness and keys are inputs, so every result can be replayed in a test or an audit.
- **Fail closed.** Unknown input, missing fields or an unmatched policy gives an error or a denial, never a silent pass.

## Part of a series

This is one of 12 repos that work through a public Forward Deployed Engineering roadmap ([source post](https://x.com/suraj_sharma14/status/2124827808535048566)). All 12: [github.com/braylo777?q=fde-](https://github.com/braylo777?tab=repositories&q=fde-).

## License

MIT — see [LICENSE](LICENSE). Built by [Braylo SI](https://braylo.si).
