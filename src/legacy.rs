//! Mock on-prem SOAP CustomerService. It speaks real SOAP XML in and out, so the bridge is
//! tested end to end through `soap::envelope` and `soap::parse_response`. Replace `call` with
//! an HTTP POST to the real endpoint in production.
//!
//! The mock keeps state so writes are real: `UpdateCustomer` changes a record and bumps its
//! `Version`, and refuses a stale `ExpectedVersion` with a Conflict fault (optimistic locking,
//! the way most 15-year-old ERPs guard concurrent edits).

use crate::xml::{self, escape};

#[derive(Clone, Debug)]
pub struct Customer {
    pub id: String,
    pub name: String,
    pub status: String,
    pub balance: String,
    pub credit_limit: String,
    pub version: u64,
}

pub struct Backend {
    pub customers: Vec<Customer>,
}

impl Default for Backend {
    fn default() -> Self {
        let c = |id: &str, name: &str, status: &str, bal: &str, lim: &str| Customer {
            id: id.into(),
            name: name.into(),
            status: status.into(),
            balance: bal.into(),
            credit_limit: lim.into(),
            version: 7,
        };
        Backend {
            customers: vec![
                c("C-1001", "Acme Foods", "ACTIVE", "12500.00", "20000.00"),
                c("C-1002", "Blue River Co", "CREDIT_HOLD", "-450.20", "5000.00"),
                c("C-1003", "Nordic Parts GmbH", "ACTIVE", "0.00", "15000.00"),
            ],
        }
    }
}

fn customer_xml(c: &Customer) -> String {
    format!(
        "<Customer><Id>{}</Id><Name>{}</Name><Status>{}</Status><Balance>{}</Balance><CreditLimit>{}</CreditLimit><Version>{}</Version></Customer>",
        c.id, escape(&c.name), c.status, c.balance, c.credit_limit, c.version
    )
}

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

/// Stateless read-only call against a fresh backend (kept for simple callers and tests).
pub fn call(request: &str) -> String {
    Backend::default().call(request)
}

impl Backend {
pub fn call(&mut self, request: &str) -> String {
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
            match self.customers.iter().find(|c| c.id == id) {
                Some(c) => wrap(&format!(
                    "<GetCustomerResponse>{}</GetCustomerResponse>",
                    customer_xml(c)
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
        "UpdateCustomer" => {
            let id = arg("CustomerId");
            let field = arg("Field");
            let value = arg("Value");
            let want: u64 = arg("ExpectedVersion").parse().unwrap_or(0);
            let Some(c) = self.customers.iter_mut().find(|c| c.id == id) else {
                return fault("Client", &format!("Customer {id} not found"));
            };
            if c.version != want {
                return fault(
                    "Client",
                    &format!("Conflict: record is at version {}, not {want}", c.version),
                );
            }
            match field.as_str() {
                "Status" => c.status = value,
                "CreditLimit" => c.credit_limit = value,
                f => return fault("Client", &format!("field {f} is not updatable")),
            }
            c.version += 1;
            wrap(&format!(
                "<UpdateCustomerResponse>{}</UpdateCustomerResponse>",
                customer_xml(c)
            ))
        }
        other => fault("Client", &format!("operation {other} not supported")),
    }
}
}
