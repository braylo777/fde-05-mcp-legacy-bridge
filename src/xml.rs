//! Minimal, strict XML reader for SOAP payloads: elements, attributes (kept for xmlns only),
//! text, CDATA, comments, processing instructions, the 5 named entities and numeric refs.
//! DOCTYPE is refused (no entity expansion, so no XXE / billion-laughs).

#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub name: String, // local name, namespace prefix removed
    pub children: Vec<Node>,
    pub text: String,
}

pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn unescape(s: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let end = rest[i..].find(';').ok_or("unterminated entity")? + i;
        let ent = &rest[i + 1..end];
        let ch = match ent {
            "lt" => '<',
            "gt" => '>',
            "amp" => '&',
            "quot" => '"',
            "apos" => '\'',
            e if e.starts_with("#x") => {
                char::from_u32(u32::from_str_radix(&e[2..], 16).map_err(|_| "bad entity")?)
                    .ok_or("bad char")?
            }
            e if e.starts_with('#') => {
                char::from_u32(e[1..].parse().map_err(|_| "bad entity")?).ok_or("bad char")?
            }
            e => return Err(format!("unknown entity &{e};")),
        };
        out.push(ch);
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

struct P<'a> {
    s: &'a str,
    i: usize,
}

impl<'a> P<'a> {
    fn rest(&self) -> &'a str {
        &self.s[self.i..]
    }
    fn skip_ws(&mut self) {
        while let Some(c) = self.rest().chars().next().filter(|c| c.is_whitespace()) {
            self.i += c.len_utf8(); // multibyte whitespace must not split a char
        }
    }
    fn skip_misc(&mut self) -> Result<(), String> {
        loop {
            self.skip_ws();
            if self.rest().starts_with("<?") {
                self.i += self.rest().find("?>").ok_or("unterminated <?")? + 2;
            } else if self.rest().starts_with("<!--") {
                self.i += self.rest().find("-->").ok_or("unterminated comment")? + 3;
            } else if self.rest().starts_with("<!DOCTYPE") {
                return Err("DOCTYPE not allowed".into());
            } else {
                return Ok(());
            }
        }
    }
    fn element(&mut self, depth: usize) -> Result<Node, String> {
        if depth > 64 {
            return Err("XML nested too deep".into());
        }
        if !self.rest().starts_with('<') {
            return Err(format!("expected < at byte {}", self.i));
        }
        let close = self.rest().find('>').ok_or("unterminated tag")?;
        let tag = &self.rest()[1..close];
        let self_closing = tag.ends_with('/');
        let tag = tag.trim_end_matches('/');
        let qname = tag
            .split_whitespace()
            .next()
            .ok_or("empty tag")?
            .to_string();
        let local = qname.rsplit(':').next().unwrap().to_string();
        self.i += close + 1;
        let mut node = Node {
            name: local,
            children: vec![],
            text: String::new(),
        };
        if self_closing {
            return Ok(node);
        }
        loop {
            if self.rest().starts_with(&format!("</{qname}")) && self.rest()[qname.len() + 2..].starts_with(|c: char| c == '>' || c.is_whitespace()) {
                self.i += self.rest().find('>').ok_or("unterminated close tag")? + 1;
                node.text = node.text.trim().to_string();
                return Ok(node);
            } else if self.rest().starts_with("<![CDATA[") {
                let end = self.rest().find("]]>").ok_or("unterminated CDATA")?;
                node.text.push_str(&self.rest()[9..end]);
                self.i += end + 3;
            } else if self.rest().starts_with("<!--") {
                self.i += self.rest().find("-->").ok_or("unterminated comment")? + 3;
            } else if self.rest().starts_with("</") {
                return Err(format!("mismatched close tag, expected </{qname}>"));
            } else if self.rest().starts_with('<') {
                node.children.push(self.element(depth + 1)?);
            } else if self.rest().is_empty() {
                return Err(format!("unexpected end inside <{qname}>"));
            } else {
                let end = self.rest().find('<').unwrap_or(self.rest().len());
                node.text.push_str(&unescape(&self.rest()[..end])?);
                self.i += end;
            }
        }
    }
}

pub fn parse(s: &str) -> Result<Node, String> {
    let mut p = P { s, i: 0 };
    p.skip_misc()?;
    let n = p.element(0)?;
    p.skip_misc()?;
    if !p.rest().trim().is_empty() {
        return Err("trailing content after root element".into());
    }
    Ok(n)
}

impl Node {
    pub fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }
    /// Leaf -> string; element -> object; repeated child names -> array.
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::{Map, Value};
        if self.children.is_empty() {
            return Value::String(self.text.clone());
        }
        let mut m: Map<String, Value> = Map::new();
        for c in &self.children {
            let v = c.to_json();
            match m.get_mut(&c.name) {
                Some(Value::Array(a))
                    if self.children.iter().filter(|x| x.name == c.name).count() > 1 =>
                {
                    a.push(v)
                }
                Some(_) => {}
                None => {
                    let repeated = self.children.iter().filter(|x| x.name == c.name).count() > 1;
                    m.insert(
                        c.name.clone(),
                        if repeated { Value::Array(vec![v]) } else { v },
                    );
                }
            }
        }
        Value::Object(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_namespaces_entities_cdata() {
        let n = parse(r#"<?xml version="1.0"?><s:Env xmlns:s="x"><s:Body><a>Tom &amp; Jerry &#233;</a><b><![CDATA[<raw>]]></b><c/></s:Body></s:Env>"#).unwrap();
        let body = n.child("Body").unwrap();
        assert_eq!(body.child("a").unwrap().text, "Tom & Jerry é");
        assert_eq!(body.child("b").unwrap().text, "<raw>");
        assert_eq!(body.child("c").unwrap().text, "");
    }
    #[test]
    fn rejects_doctype_and_mismatch() {
        assert!(parse("<!DOCTYPE x [<!ENTITY a 'b'>]><x/>").is_err());
        assert!(parse("<a><b></a>").is_err());
        assert!(parse("<a>").is_err());
        assert!(parse("<a/><b/>").is_err());
    }
    #[test]
    fn multibyte_whitespace_and_prefix_close_tags() {
        assert!(parse("\u{3000}<a>x</a>").is_ok());
        assert!(parse("<a>é</a>\u{2003}").is_ok());
        assert!(parse("<a>x</ab>").is_err());
    }
    #[test]
    fn repeated_children_become_arrays() {
        let n = parse("<r><o>1</o><o>2</o><x>y</x></r>").unwrap();
        assert_eq!(n.to_json(), serde_json::json!({"o": ["1", "2"], "x": "y"}));
    }
    #[test]
    fn escape_round_trip() {
        let s = "a<b>&\"c'";
        assert_eq!(parse(&format!("<x>{}</x>", escape(s))).unwrap().text, s);
    }
}
