//! testrs — a pytest-like Rust test framework with compile-time, type-based
//! fixture dependency injection.
//!
//! Fixtures and tests are declared with the [`fixture`] and [`test`] attribute
//! macros. These are thin markers: they emit the annotated function unchanged
//! plus discoverable metadata. The `testrs` CLI reads that metadata together
//! with resolved type information (via rustdoc JSON) to build a fixture
//! dependency graph and generate a [`kitest`](https://docs.rs/kitest) harness.

pub use testrs_macros::{fixture, test};
