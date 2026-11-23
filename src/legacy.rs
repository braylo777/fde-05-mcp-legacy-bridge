//! Mock on-prem SOAP CustomerService. It speaks real SOAP XML in and out, so the bridge is
//! tested end to end through `soap::envelope` and `soap::parse_response`. Replace `call` with
//! an HTTP POST to the real endpoint in production.

use crate::xml::{self, escape};

const CUSTOMERS: &[(&str, &str, &str, &str)] = &[
    ("C-1001", "Acme Foods", "ACTIVE", "12500.00"),
    ("C-1002", "Blue River Co", "CREDIT_HOLD", "-450.20"),
    ("C-1003", "Nordic Parts GmbH", "ACTIVE", "0.00"),
];
const ORDERS: &[(&str, &str, &str, &str)] = &[
    ("C-1001", "SO-77", "2026-10-02", "OPEN"),
    ("C-1001", "SO-81", "2026-11-14", "SHIPPED"),
    ("C-1003", "SO-90", "2026-11-20", "OPEN"),
];

fn wrap(inner: &str) -> String {
    format!("<?xml version=\"1.0\"?><soap:Envelope xmlns:soap=\"http://schemas.xmlsoap.org/soap/envelope/\"><soap:Body>{inner}</soap:Body></soap:Envelope>")
}

fn fault(code: &str, msg: &str) -> String {
    wrap(&format!(
        "<soap:Fault><faultcode>soap:{code}</faultcode><faultstring>{}</faultstring></soap:Fault>",
        escape(msg)
    ))
}

pub fn call(request: &str) -> String {
    let Ok(env) = xml::parse(request) else {
        return fault("Client", "malformed XML");
    };
    let Some(op) = env.child("Body").and_then(|b| b.children.first()) else {
        return fault("Client", "no operation");
    };
    let arg = |n: &str| op.child(n).map(|c| c.text.clone()).unwrap_or_default();
    match op.name.as_str() {
        "GetCustomer" => {
            let id = arg("CustomerId");
            match CUSTOMERS.iter().find(|c| c.0 == id) {
                Some(c) => wrap(&format!(
                    "<GetCustomerResponse><Customer><Id>{}</Id><Name>{}</Name><Status>{}</Status><Balance>{}</Balance></Customer></GetCustomerResponse>",
                    c.0, escape(c.1), c.2, c.3
                )),
                None => fault("Client", &format!("Customer {id} not found")),
            }
        }
        "ListOrders" => {
            let id = arg("CustomerId");
            let rows: String = ORDERS
                .iter()
                .filter(|o| o.0 == id)
                .map(|o| {
                    format!(
                        "<Order><No>{}</No><Date>{}</Date><Status>{}</Status></Order>",
                        o.1, o.2, o.3
                    )
                })
                .collect();
            wrap(&format!(
                "<ListOrdersResponse><Orders>{rows}</Orders></ListOrdersResponse>"
            ))
        }
        other => fault("Client", &format!("operation {other} not supported")),
    }
}
