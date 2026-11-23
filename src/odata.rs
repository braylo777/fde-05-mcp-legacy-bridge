//! SAP OData v2 query builder. Agents pass structured filters, never raw `$filter` strings,
//! so a value like `x' or 1 eq 1 or 'y` stays a literal.

use crate::str_field;
use serde_json::{json, Value};

fn is_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !s.starts_with(|c: char| c.is_ascii_digit())
}

fn pct(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~'
            | b'\''
            | b'('
            | b')'
            | b','
            | b'$' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn literal(v: &Value) -> Result<String, String> {
    match v {
        Value::String(s) => Ok(format!("'{}'", s.replace('\'', "''"))),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        _ => Err("filter values must be string, number or bool".into()),
    }
}

pub fn build_op(i: &Value) -> Result<Value, String> {
    let base = str_field(i, "service_url")?.trim_end_matches('/');
    let set = str_field(i, "entity_set")?;
    if !is_ident(set) {
        return Err("entity_set must be an identifier".into());
    }
    let max_top = i["max_top"].as_u64().unwrap_or(100);
    let top = i["top"].as_u64().unwrap_or(20).min(max_top);
    let mut q = vec![format!("$top={top}"), "$format=json".to_string()];
    if let Some(sel) = i["select"].as_array() {
        let cols: Vec<&str> = sel.iter().filter_map(Value::as_str).collect();
        if let Some(bad) = cols.iter().find(|c| !is_ident(c)) {
            return Err(format!("bad $select column `{bad}`"));
        }
        q.push(format!("$select={}", cols.join(",")));
    }
    if let Some(fs) = i["filters"].as_array() {
        let mut parts = Vec::new();
        for f in fs {
            let field = str_field(f, "field")?;
            if !is_ident(field) {
                return Err(format!("bad filter field `{field}`"));
            }
            let op = str_field(f, "op")?;
            if !["eq", "ne", "gt", "ge", "lt", "le"].contains(&op) {
                return Err(format!("operator `{op}` not allowed"));
            }
            parts.push(format!("{field} {op} {}", literal(&f["value"])?));
        }
        if !parts.is_empty() {
            q.push(format!("$filter={}", pct(&parts.join(" and "))));
        }
    }
    Ok(
        json!({"url": format!("{base}/{set}?{}", q.join("&")), "top_capped": i["top"].as_u64().unwrap_or(0) > max_top}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn injection_stays_literal() {
        let r = build_op(
            &json!({"service_url": "https://sap/odata/", "entity_set": "A_SalesOrder",
            "filters": [{"field": "SoldToParty", "op": "eq", "value": "x' or 1 eq 1 or 'y"}]}),
        )
        .unwrap();
        let u = r["url"].as_str().unwrap();
        assert!(u.contains("'x''%20or%201%20eq%201%20or%20''y'"));
    }
    #[test]
    fn rejects_bad_operator_and_field() {
        assert!(build_op(&json!({"service_url": "u", "entity_set": "S", "filters": [{"field": "a", "op": "substringof", "value": 1}]})).is_err());
        assert!(
            build_op(&json!({"service_url": "u", "entity_set": "S", "select": ["a;drop"]}))
                .is_err()
        );
    }
    #[test]
    fn caps_top() {
        let r = build_op(&json!({"service_url": "u", "entity_set": "S", "top": 5000})).unwrap();
        assert!(r["url"].as_str().unwrap().contains("$top=100"));
        assert_eq!(r["top_capped"], true);
    }
}
