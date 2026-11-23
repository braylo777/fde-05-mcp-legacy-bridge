//! SOAP 1.1 <-> JSON translation.

use crate::xml::{self, escape};
use serde_json::{json, Value};

fn is_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .next()
            .map(|c| c.is_ascii_alphabetic() || c == '_')
            .unwrap_or(false)
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

fn to_xml(name: &str, v: &Value, out: &mut String) -> Result<(), String> {
    if !is_name(name) {
        return Err(format!("`{name}` is not a valid XML element name"));
    }
    match v {
        Value::Null => out.push_str(&format!("<{name} xsi:nil=\"true\"/>")),
        Value::Array(a) => {
            for x in a {
                to_xml(name, x, out)?;
            }
        }
        Value::Object(m) => {
            out.push_str(&format!("<{name}>"));
            for (k, x) in m {
                to_xml(k, x, out)?;
            }
            out.push_str(&format!("</{name}>"));
        }
        Value::String(s) => out.push_str(&format!("<{name}>{}</{name}>", escape(s))),
        other => out.push_str(&format!("<{name}>{other}</{name}>")),
    }
    Ok(())
}

/// Build a SOAP 1.1 request. Values are XML-escaped; element names are validated, so a
/// JSON key can never inject markup into the envelope.
pub fn envelope(operation: &str, ns: &str, params: &Value) -> Result<String, String> {
    if !is_name(operation) {
        return Err("operation is not a valid XML name".into());
    }
    let mut body = String::new();
    if let Value::Object(m) = params {
        for (k, v) in m {
            to_xml(k, v, &mut body)?;
        }
    }
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><soap:Envelope xmlns:soap=\"http://schemas.xmlsoap.org/soap/envelope/\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\"><soap:Body><m:{operation} xmlns:m=\"{}\">{body}</m:{operation}></soap:Body></soap:Envelope>",
        escape(ns)
    ))
}

/// Parse a SOAP response. A `Fault` becomes `{"fault": {code, message}}`; otherwise the
/// first element in Body is returned as JSON.
pub fn parse_response(x: &str) -> Result<Value, String> {
    let root = xml::parse(x)?;
    if root.name != "Envelope" {
        return Err("not a SOAP Envelope".into());
    }
    let body = root.child("Body").ok_or("SOAP Envelope has no Body")?;
    let first = body.children.first().ok_or("empty SOAP Body")?;
    if first.name == "Fault" {
        let f = |n: &str| first.child(n).map(|c| c.text.clone()).unwrap_or_default();
        return Ok(
            json!({"fault": {"code": f("faultcode"), "message": f("faultstring"), "detail": first.child("detail").map(|d| d.to_json())}}),
        );
    }
    Ok(json!({"operation": first.name, "data": first.to_json()}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn envelope_escapes_values() {
        let e = envelope(
            "GetCustomer",
            "urn:crm",
            &json!({"id": "1<2&3", "opts": {"full": true}}),
        )
        .unwrap();
        assert!(e.contains("<id>1&lt;2&amp;3</id>"));
        assert!(e.contains("<opts><full>true</full></opts>"));
        assert!(xml::parse(&e).is_ok());
    }
    #[test]
    fn bad_key_rejected() {
        assert!(envelope("Op", "u", &json!({"a><evil": 1})).is_err());
        assert!(envelope("Op><x", "u", &json!({})).is_err());
    }
    #[test]
    fn fault_is_detected() {
        let r = parse_response("<s:Envelope xmlns:s='x'><s:Body><s:Fault><faultcode>s:Client</faultcode><faultstring>Not found</faultstring></s:Fault></s:Body></s:Envelope>").unwrap();
        assert_eq!(r["fault"]["message"], "Not found");
    }
}
