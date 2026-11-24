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
    pub backend: legacy::Backend,
    /// confirm_token -> (args that were previewed, expiry ms). One use each.
    pending: HashMap<String, (Value, u64)>,
    /// idempotency_key -> committed result, so a retried commit never writes twice.
    committed: HashMap<String, (Value, Value)>,
}

/// How long a preview's confirm token stays valid.
pub const CONFIRM_TTL_MS: u64 = 60_000;
/// Credit-limit changes above either bound are refused and routed to a human.
pub const MAX_LIMIT_STEP_CENTS: i64 = 1_000_000;
pub const MAX_LIMIT_STEP_PCT: i64 = 50;

impl Default for Server {
    fn default() -> Self {
        Server {
            initialized: false,
            buckets: HashMap::new(),
            clock_ms: 0,
            audit: vec![],
            backend: legacy::Backend::default(),
            pending: HashMap::new(),
            committed: HashMap::new(),
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
         "annotations": {"readOnlyHint": true}},
        {"name": "legacy_update_customer",
         "description": "Change one field (Status or CreditLimit) on a CRM customer. Two steps: mode=preview returns the diff and a one-use confirm_token; mode=commit with that token and an idempotency_key writes it. Stale expected_version is refused.",
         "inputSchema": {"type": "object", "properties": {
             "customer_id": {"type": "string", "pattern": "^C-[0-9]{4}$"},
             "field": {"type": "string", "enum": ["Status", "CreditLimit"]},
             "value": {"type": "string"},
             "expected_version": {"type": "integer"},
             "mode": {"type": "string", "enum": ["preview", "commit"]},
             "confirm_token": {"type": "string"},
             "idempotency_key": {"type": "string"}},
             "required": ["customer_id", "field", "value", "expected_version", "mode"],
             "additionalProperties": false},
         "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true}}
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
        if let Some(allowed) = p["enum"].as_array() {
            if !allowed.contains(v) {
                return Err(format!("argument `{k}` must be one of {}", p["enum"]));
            }
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

/// Amount "12500.00" -> cents. None for negatives, junk or more than 2 decimals.
fn cents(s: &str) -> Option<i64> {
    let (w, f) = s.split_once('.').unwrap_or((s, "0"));
    if w.is_empty() || w.len() > 12 || f.len() > 2 || !w.chars().chain(f.chars()).all(|c| c.is_ascii_digit()) {
        return None;
    }
    let f: i64 = format!("{f:0<2}").parse().ok()?;
    Some(w.parse::<i64>().ok()? * 100 + f)
}

fn confirm_token(a: &Value, now: u64) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(format!("{a}|{now}").as_bytes())
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
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
        match name {
            "legacy_get_customer" | "legacy_list_orders" => {
                let op = if name == "legacy_get_customer" { "GetCustomer" } else { "ListOrders" };
                let id = args["customer_id"].as_str().unwrap_or("");
                match self.soap(op, json!({"CustomerId": id})) {
                    Ok(d) => tool_result(d.to_string(), d, false),
                    Err((m, d)) => tool_result(m, d, true),
                }
            }
            "legacy_update_customer" => self.update_customer(args),
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

    /// One SOAP round trip to the backend. Ok(data) or Err((message, structured)).
    fn soap(&mut self, op: &str, params: Value) -> Result<Value, (String, Value)> {
        let req = soap::envelope(op, "urn:acme:crm:v1", &params).map_err(|e| (e, json!({})))?;
        match soap::parse_response(&self.backend.call(&req)) {
            Ok(r) if r.get("fault").is_some() => {
                Err((format!("Legacy fault: {}", r["fault"]["message"]), r))
            }
            Ok(r) => Ok(r["data"].clone()),
            Err(e) => Err((format!("Bad SOAP response: {e}"), json!({}))),
        }
    }

    /// Safe write: validate, preview with a one-use token, commit once per idempotency key.
    fn update_customer(&mut self, a: &Value) -> Value {
        let id = a["customer_id"].as_str().unwrap_or("");
        let field = a["field"].as_str().unwrap_or("");
        let value = a["value"].as_str().unwrap_or("");
        let want = a["expected_version"].as_u64().unwrap_or(0);
        let fail = |m: String, d: Value| tool_result(m, d, true);
        if a["mode"] == "commit" {
            let key = a["idempotency_key"].as_str().unwrap_or("");
            if key.is_empty() {
                return fail("commit needs an idempotency_key".into(), json!({}));
            }
            if let Some((prev_args, prev)) = self.committed.get(key) {
                let same = ["customer_id", "field", "value", "expected_version"]
                    .iter()
                    .all(|k| prev_args[*k] == a[*k]);
                if !same {
                    return fail(format!("idempotency_key {key} was already used for a different change"), json!({}));
                }
                let mut r = prev.clone();
                r["structuredContent"]["replayed"] = json!(true);
                r["content"][0]["text"] = json!(format!("Replayed earlier commit for key {key}; nothing written"));
                return r;
            }
            let tok = a["confirm_token"].as_str().unwrap_or("");
            let Some((previewed, exp)) = self.pending.remove(tok) else {
                return fail("commit needs the confirm_token from a preview (tokens are one-use)".into(), json!({}));
            };
            let same = ["customer_id", "field", "value", "expected_version"]
                .iter()
                .all(|k| previewed[*k] == a[*k]);
            if !same {
                return fail("arguments differ from the preview; preview again".into(), json!({}));
            }
            if self.clock_ms > exp {
                return fail("confirm_token expired; preview again".into(), json!({}));
            }
            // Write the normalized amount ("25000.5" -> "25000.50").
            let value = match (field, cents(value)) {
                ("CreditLimit", Some(c)) => format!("{}.{:02}", c / 100, c % 100),
                _ => value.to_string(),
            };
            self.pending.retain(|_, (_, exp)| *exp >= self.clock_ms);
            let r = match self.soap("UpdateCustomer", json!({"CustomerId": id, "Field": field,
                "Value": value, "ExpectedVersion": want.to_string()}))
            {
                Ok(d) => tool_result(format!("Updated {id}.{field}"),
                    json!({"committed": true, "record": d["Customer"]}), false),
                Err((m, d)) => fail(m, d),
            };
            if r["isError"] == false {
                self.committed.insert(key.to_string(), (a.clone(), r.clone()));
            }
            return r;
        }
        // preview
        let cur = match self.soap("GetCustomer", json!({"CustomerId": id})) {
            Ok(d) => d["Customer"].clone(),
            Err((m, d)) => return fail(m, d),
        };
        let ver: u64 = cur["Version"].as_str().and_then(|v| v.parse().ok()).unwrap_or(0);
        if ver != want {
            return fail(format!("Conflict: record is at version {ver}, not {want}; re-read it"),
                json!({"conflict": true, "current": cur}));
        }
        let before = cur[field].as_str().unwrap_or("").to_string();
        match field {
            "Status" if !["ACTIVE", "CREDIT_HOLD"].contains(&value) => {
                return fail("Status must be ACTIVE or CREDIT_HOLD".into(), json!({}));
            }
            "CreditLimit" => {
                let (Some(old), Some(new)) = (cents(&before), cents(value)) else {
                    return fail("CreditLimit must be a non-negative amount like 25000.00".into(), json!({}));
                };
                let step = (new - old).abs();
                if step > MAX_LIMIT_STEP_CENTS || (old > 0 && step * 100 > old * MAX_LIMIT_STEP_PCT) {
                    return fail(format!("Change of {:.2} is over the agent limit; route to a human approver", step as f64 / 100.0),
                        json!({"approval_required": true, "before": before, "after": value}));
                }
            }
            _ => {}
        }
        let token = confirm_token(a, self.clock_ms);
        self.pending.insert(token.clone(), (a.clone(), self.clock_ms + CONFIRM_TTL_MS));
        tool_result(format!("Preview: {id}.{field} {before} -> {value}. Commit with confirm_token."),
            json!({"preview": true, "diff": {"field": field, "before": before, "after": value},
                   "version": ver, "confirm_token": token, "expires_at_ms": self.clock_ms + CONFIRM_TTL_MS}), false)
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

    fn upd(s: &mut Server, args: Value) -> Value {
        let m = json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call", "params": {"name": "legacy_update_customer", "arguments": args}});
        let r: Value = serde_json::from_str(&s.handle(&m.to_string()).unwrap()).unwrap();
        r["result"].clone()
    }
    fn base(mode: &str) -> Value {
        json!({"customer_id": "C-1001", "field": "CreditLimit", "value": "25000.00", "expected_version": 7, "mode": mode})
    }

    #[test]
    fn write_needs_preview_then_commits_once() {
        let mut s = Server::default();
        init(&mut s);
        let mut c = base("commit");
        c["idempotency_key"] = json!("k1");
        c["confirm_token"] = json!("forged");
        assert_eq!(upd(&mut s, c.clone())["isError"], true);
        let p = upd(&mut s, base("preview"));
        assert_eq!(p["isError"], false);
        assert_eq!(p["structuredContent"]["diff"]["before"], "20000.00");
        c["confirm_token"] = p["structuredContent"]["confirm_token"].clone();
        let r = upd(&mut s, c.clone());
        assert_eq!(r["structuredContent"]["record"]["CreditLimit"], "25000.00");
        assert_eq!(r["structuredContent"]["record"]["Version"], "8");
        // Retry with the same key (after the rate-limit window): same result, no second write.
        s.clock_ms += 2_000;
        let again = upd(&mut s, c);
        assert_eq!(again["structuredContent"]["replayed"], true);
        assert_eq!(s.backend.customers[0].version, 8);
    }

    #[test]
    fn stale_version_and_big_jumps_and_changed_args_refused() {
        let mut s = Server::default();
        init(&mut s);
        let mut stale = base("preview");
        stale["expected_version"] = json!(6);
        assert_eq!(upd(&mut s, stale)["structuredContent"]["conflict"], true);
        let mut big = base("preview");
        big["value"] = json!("90000.00");
        assert_eq!(upd(&mut s, big)["structuredContent"]["approval_required"], true);
        s.clock_ms += 3_000;
        let p = upd(&mut s, base("preview"));
        let mut c = base("commit");
        c["value"] = json!("29000.00");
        c["idempotency_key"] = json!("k2");
        c["confirm_token"] = p["structuredContent"]["confirm_token"].clone();
        assert_eq!(upd(&mut s, c)["isError"], true);
        assert_eq!(s.backend.customers[0].credit_limit, "20000.00");
    }

    #[test]
    fn expired_token_and_bad_enum_refused() {
        let mut s = Server::default();
        init(&mut s);
        let p = upd(&mut s, base("preview"));
        s.clock_ms += CONFIRM_TTL_MS + 1;
        let mut c = base("commit");
        c["idempotency_key"] = json!("k3");
        c["confirm_token"] = p["structuredContent"]["confirm_token"].clone();
        assert!(upd(&mut s, c)["content"][0]["text"].as_str().unwrap().contains("expired"));
        let m = json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call", "params": {"name": "legacy_update_customer",
            "arguments": {"customer_id": "C-1001", "field": "Balance", "value": "0", "expected_version": 7, "mode": "preview"}}});
        assert!(s.handle(&m.to_string()).unwrap().contains("-32602"));
    }

    #[test]
    fn cents_parser() {
        assert_eq!(cents("25000.5"), Some(2_500_050));
        assert_eq!(cents("-1.00"), None);
        assert_eq!(cents("1.234"), None);
        assert_eq!(cents("abc"), None);
    }

    #[test]
    fn reused_key_with_other_change_refused_and_amount_normalized() {
        let mut s = Server::default();
        init(&mut s);
        let mut b = base("preview");
        b["value"] = json!("25000.5");
        let p = upd(&mut s, b.clone());
        let mut c = b.clone();
        c["mode"] = json!("commit");
        c["idempotency_key"] = json!("k9");
        c["confirm_token"] = p["structuredContent"]["confirm_token"].clone();
        assert_eq!(upd(&mut s, c.clone())["isError"], false);
        assert_eq!(s.backend.customers[0].credit_limit, "25000.50");
        s.clock_ms += 2_000;
        let mut other = c;
        other["customer_id"] = json!("C-1002");
        assert_eq!(upd(&mut s, other)["isError"], true);
    }
}
