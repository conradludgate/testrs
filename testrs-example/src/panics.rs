//! Tests asserted to panic.
//!
//! `#[panics]` passes if the test panics at all; `#[panics("…")]` additionally
//! requires the panic message to contain the given substring.

use testrs::{panics, test};

/// Stand-in for code under test that's *supposed* to abort on bad input.
fn parse_port(s: &str) -> u16 {
    let port: u16 = s.parse().expect("port must be a number");
    assert!(port != 0, "port must be non-zero");
    port
}

#[test]
#[panics]
fn rejects_non_numeric_port() {
    parse_port("https");
}

#[test]
#[panics("non-zero")]
fn rejects_zero_port() {
    parse_port("0");
}
