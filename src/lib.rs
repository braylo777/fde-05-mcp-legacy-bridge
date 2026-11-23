//! Stage 5 — MCP & Legacy System Integration.
//!
//! * `mcp`     — a Model Context Protocol server written against raw JSON-RPC 2.0 (no SDK):
//!   `initialize`, `ping`, `tools/list`, `tools/call`, notifications, standard error codes,
//!   and input-schema validation before any legacy call.
//! * `xml` / `soap` — SOAP 1.1 envelopes from JSON, and SOAP responses / Faults back to JSON.
//! * `legacy`  — a mock SOAP CustomerService that stands in for the customer's on-prem system.
//! * `limit`   — token-bucket rate limiting per tool, plus retry planning with exponential
//!   backoff, jitter and `Retry-After`.
//! * `odata`   — safe SAP OData v2 query URL building (quote escaping, $select / $top limits).

mod api;
pub mod legacy;
pub mod limit;
pub mod mcp;
pub mod odata;
pub mod soap;
pub mod xml;

pub use api::dispatch;
use serde_json::{json, Value};

pub const OPS: &[&str] = &["mcp", "json_to_soap", "soap_to_json", "retry_plan", "odata_query"];

pub(crate) fn str_field<'a>(v: &'a Value, k: &str) -> Result<&'a str, String> {
    v.get(k).and_then(Value::as_str).ok_or_else(|| format!("missing string field `{k}`"))
}

pub fn handle(op: &str, i: &Value) -> Result<Value, String> {
    match op {
        "mcp" => mcp::session_op(i),
        "json_to_soap" => Ok(json!({"envelope": soap::envelope(
            str_field(i, "operation")?, str_field(i, "namespace")?, i.get("params").unwrap_or(&Value::Null))?})),
        "soap_to_json" => soap::parse_response(str_field(i, "xml")?),
        "retry_plan" => limit::retry_plan_op(i),
        "odata_query" => odata::build_op(i),
        _ => Err(format!("unknown op `{op}`; known: {}", OPS.join(", "))),
    }
}
