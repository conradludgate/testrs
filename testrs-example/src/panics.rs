//! Tests asserted to panic.
//!
//! `#[test(should_panic)]` passes if the test panics at all;
//! `#[test(should_panic = "…")]` additionally requires the panic message to
//! contain the given substring.

use testrs::test;

/// Stand-in for code under test that's *supposed* to abort on bad input.
fn parse_port(s: &str) -> u16 {
    let port: u16 = s.parse().expect("port must be a number");
    assert!(port != 0, "port must be non-zero");
    port
}

#[test(should_panic)]
fn rejects_non_numeric_port() {
    parse_port("https");
}

#[test(should_panic = "non-zero")]
fn rejects_zero_port() {
    parse_port("0");
}
