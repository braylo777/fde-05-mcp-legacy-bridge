//! MCP server over raw JSON-RPC 2.0. Transport-agnostic: `Server::handle` takes one message
//! (or a batch array) as a string and returns the response string, or None for notifications.
//! The CLI wires it to stdio (`fde-05-mcp-legacy-bridge serve`), one JSON message per line.

use crate::{legacy, limit::Bucket, odata, soap};
use serde_json::{json, Value};
use std::collections::HashMap;

pub const PROTOCOL: &str = "2025-06-18";

pub struct Server {
    initialized: bool,
    buckets: HashMap<String, Bucket>,
    pub clock_ms: u64,
    pub audit: Vec<Value>,
}

impl Default for Server {
    fn default() -> Self {
        Server {
            initialized: false,
            buckets: HashMap::new(),
            clock_ms: 0,
            audit: vec![],
        }
    }
}

fn tools() -> Value {
    json!([
        {"name": "legacy_get_customer", "description": "Read one customer from the on-prem CRM (SOAP GetCustomer).",
         "inputSchema": {"type": "object", "properties": {"customer_id": {"type": "string", "pattern": "^C-[0-9]{4}$"}},
                         "required": ["customer_id"], "additionalProperties": false},
         "annotations": {"readOnlyHint": true}},
        {"name": "legacy_list_orders", "description": "List a customer's orders from the on-prem CRM (SOAP ListOrders).",
         "inputSchema": {"type": "object", "properties": {"customer_id": {"type": "string", "pattern": "^C-[0-9]{4}$"}},
                         "required": ["customer_id"], "additionalProperties": false},
         "annotations": {"readOnlyHint": true}},
        {"name": "sap_build_query", "description": "Build a safe SAP OData v2 URL for an entity set with typed filters.",
         "inputSchema": {"type": "object", "properties": {
             "entity_set": {"type": "string"}, "top": {"type": "integer"},
             "select": {"type": "array"}, "filters": {"type": "array"}},
             "required": ["entity_set"], "additionalProperties": false},
         "annotations": {"readOnlyHint": true}}
    ])
}

/// Small JSON-Schema subset: type object, required, additionalProperties:false,
/// property types string / integer / array, and ^C-[0-9]{4}$ style patterns.
pub fn validate(schema: &Value, args: &Value) -> Result<(), String> {
    let obj = args.as_object().ok_or("arguments must be an object")?;
    for r in schema["required"].as_array().into_iter().flatten() {
        let k = r.as_str().unwrap_or("");
        if !obj.contains_key(k) {
            return Err(format!("missing required argument `{k}`"));
        }
    }
    let props = schema["properties"].as_object();
    for (k, v) in obj {
        let Some(p) = props.and_then(|p| p.get(k)) else {
            if schema["additionalProperties"] == false {
                return Err(format!("unexpected argument `{k}`"));
            }
            continue;
        };
        let ok = match p["type"].as_str() {
            Some("string") => v.is_string(),
            Some("integer") => v.is_i64() || v.is_u64(),
            Some("array") => v.is_array(),
            _ => true,
        };
        if !ok {
            return Err(format!("argument `{k}` must be {}", p["type"]));
        }
        if p["pattern"] == "^C-[0-9]{4}$" {
            let s = v.as_str().unwrap_or("");
            if !(s.len() == 6 && s.starts_with("C-") && s[2..].chars().all(|c| c.is_ascii_digit()))
            {
                return Err(format!("argument `{k}` must match ^C-[0-9]{{4}}$"));
            }
        }
    }
    Ok(())
}

fn err(id: &Value, code: i64, msg: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": msg}})
}

fn tool_result(text: String, structured: Value, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "structuredContent": structured, "isError": is_error})
}

impl Server {
    fn call_tool(&mut self, name: &str, args: &Value) -> Value {
        let bucket = self
            .buckets
            .entry(name.to_string())
            .or_insert_with(|| Bucket::new(3, 1));
        if let Err(wait) = bucket.take(self.clock_ms) {
            return tool_result(
                format!(
                    "Rate limited: legacy system allows 3 calls burst, 1/sec. Retry in {wait} ms."
                ),
                json!({"rate_limited": true, "retry_after_ms": wait}),
                true,
            );
        }
        let soap_call = |op: &str, id: &str| -> Value {
            let req = match soap::envelope(op, "urn:acme:crm:v1", &json!({"CustomerId": id})) {
                Ok(r) => r,
                Err(e) => return tool_result(e, json!({}), true),
            };
            match soap::parse_response(&legacy::call(&req)) {
                Ok(r) if r.get("fault").is_some() => {
                    tool_result(format!("Legacy fault: {}", r["fault"]["message"]), r, true)
                }
                Ok(r) => tool_result(r["data"].to_string(), r["data"].clone(), false),
                Err(e) => tool_result(format!("Bad SOAP response: {e}"), json!({}), true),
            }
        };
        match name {
            "legacy_get_customer" => {
                soap_call("GetCustomer", args["customer_id"].as_str().unwrap_or(""))
            }
            "legacy_list_orders" => {
                soap_call("ListOrders", args["customer_id"].as_str().unwrap_or(""))
            }
            "sap_build_query" => {
                let mut a = args.clone();
                a["service_url"] =
                    json!("https://sap.acme.internal/sap/opu/odata/sap/API_SALES_ORDER_SRV");
                match odata::build_op(&a) {
                    Ok(r) => tool_result(r["url"].as_str().unwrap_or("").to_string(), r, false),
                    Err(e) => tool_result(e, json!({}), true),
                }
            }
            _ => tool_result(format!("unknown tool {name}"), json!({}), true),
        }
    }

    fn one(&mut self, m: &Value) -> Option<Value> {
        let id = m.get("id").cloned();
        if m["jsonrpc"] != "2.0" || !m["method"].is_string() {
            return Some(err(&id.unwrap_or(Value::Null), -32600, "Invalid Request"));
        }
        let method = m["method"].as_str().unwrap();
        let Some(id) = id else {
            // Notification: never answered.
            if method == "notifications/initialized" {
                self.initialized = true;
            }
            return None;
        };
        let p = &m["params"];
        Some(match method {
            "initialize" => {
                self.initialized = true;
                json!({"jsonrpc": "2.0", "id": id, "result": {
                    "protocolVersion": PROTOCOL,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "fde-05-mcp-legacy-bridge", "version": env!("CARGO_PKG_VERSION")}}})
            }
            "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
            _ if !self.initialized => err(&id, -32002, "Server not initialized"),
            "tools/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"tools": tools()}}),
            "tools/call" => {
                let name = p["name"].as_str().unwrap_or("");
                let args = p.get("arguments").cloned().unwrap_or(json!({}));
                let Some(tool) = tools()
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|t| t["name"] == name)
                    .cloned()
                else {
                    return Some(err(&id, -32602, &format!("Unknown tool: {name}")));
                };
                if let Err(e) = validate(&tool["inputSchema"], &args) {
                    return Some(err(&id, -32602, &format!("Invalid arguments: {e}")));
                }
                let r = self.call_tool(name, &args);
                self.audit.push(json!({"at_ms": self.clock_ms, "tool": name, "args": args, "is_error": r["isError"]}));
                json!({"jsonrpc": "2.0", "id": id, "result": r})
            }
            _ => err(&id, -32601, &format!("Method not found: {method}")),
        })
    }

    pub fn handle(&mut self, line: &str) -> Option<String> {
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => return Some(err(&Value::Null, -32700, "Parse error").to_string()),
        };
        match v {
            Value::Array(a) if a.is_empty() => {
                Some(err(&Value::Null, -32600, "Invalid Request").to_string())
            }
            Value::Array(a) => {
                let out: Vec<Value> = a.iter().filter_map(|m| self.one(m)).collect();
                if out.is_empty() {
                    None
                } else {
                    Some(Value::Array(out).to_string())
                }
            }
            m => self.one(&m).map(|r| r.to_string()),
        }
    }
}

/// Replay a session: `{"messages": [{"at_ms": 0, "msg": {...}}, ...]}` (or bare messages).
pub fn session_op(i: &Value) -> Result<Value, String> {
    let mut s = Server::default();
    let mut transcript = Vec::new();
    for item in i["messages"]
        .as_array()
        .ok_or("`messages` must be a list")?
    {
        let (at, msg) = match item.get("msg") {
            Some(m) => (item["at_ms"].as_u64().unwrap_or(s.clock_ms), m.clone()),
            None => (s.clock_ms, item.clone()),
        };
        s.clock_ms = s.clock_ms.max(at);
        let raw = if let Value::String(text) = &msg {
            text.clone()
        } else {
            msg.to_string()
        };
        let resp = s
            .handle(&raw)
            .map(|r| serde_json::from_str::<Value>(&r).unwrap());
        transcript.push(json!({"at_ms": s.clock_ms, "request": msg, "response": resp}));
    }
    Ok(json!({"transcript": transcript, "audit_log": s.audit}))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn init(s: &mut Server) {
        s.handle(r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{}}"#);
    }
    fn call(s: &mut Server, args: Value) -> Value {
        let m = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "legacy_get_customer", "arguments": args}});
        serde_json::from_str(&s.handle(&m.to_string()).unwrap()).unwrap()
    }
    #[test]
    fn requires_initialize() {
        let mut s = Server::default();
        let r = s
            .handle(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
            .unwrap();
        assert!(r.contains("-32002"));
    }
    #[test]
    fn parse_error_and_unknown_method() {
        let mut s = Server::default();
        assert!(s.handle("{nope").unwrap().contains("-32700"));
        init(&mut s);
        assert!(s
            .handle(r#"{"jsonrpc":"2.0","id":2,"method":"x/y"}"#)
            .unwrap()
            .contains("-32601"));
    }
    #[test]
    fn notification_gets_no_reply() {
        let mut s = Server::default();
        assert!(s
            .handle(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .is_none());
    }
    #[test]
    fn tool_call_round_trips_through_soap() {
        let mut s = Server::default();
        init(&mut s);
        let r = call(&mut s, json!({"customer_id": "C-1002"}));
        assert_eq!(
            r["result"]["structuredContent"]["Customer"]["Status"],
            "CREDIT_HOLD"
        );
        assert_eq!(r["result"]["isError"], false);
    }
    #[test]
    fn soap_fault_is_tool_error() {
        let mut s = Server::default();
        init(&mut s);
        let r = call(&mut s, json!({"customer_id": "C-9999"}));
        assert_eq!(r["result"]["isError"], true);
    }
    #[test]
    fn schema_blocks_bad_args() {
        let mut s = Server::default();
        init(&mut s);
        assert_eq!(
            call(&mut s, json!({"customer_id": "C-1' OR 1=1"}))["error"]["code"],
            -32602
        );
        assert_eq!(
            call(&mut s, json!({"customer_id": "C-1001", "x": 1}))["error"]["code"],
            -32602
        );
    }
    #[test]
    fn rate_limit_kicks_in() {
        let mut s = Server::default();
        init(&mut s);
        for _ in 0..3 {
            assert_eq!(
                call(&mut s, json!({"customer_id": "C-1001"}))["result"]["isError"],
                false
            );
        }
        assert_eq!(
            call(&mut s, json!({"customer_id": "C-1001"}))["result"]["structuredContent"]
                ["rate_limited"],
            true
        );
        s.clock_ms = 1000;
        assert_eq!(
            call(&mut s, json!({"customer_id": "C-1001"}))["result"]["isError"],
            false
        );
    }
    #[test]
    fn batch() {
        let mut s = Server::default();
        let r = s.handle(r#"[{"jsonrpc":"2.0","id":1,"method":"ping"},{"jsonrpc":"2.0","method":"notifications/initialized"}]"#).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&r)
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
}
