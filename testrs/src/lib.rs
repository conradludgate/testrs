//! testrs — a pytest-like Rust test framework with compile-time, type-based
//! fixture dependency injection.
//!
//! Fixtures and tests are declared with the [`fixture`] and [`test`] attribute
//! macros. These are thin markers: they emit the annotated function unchanged
//! plus discoverable metadata. The `testrs` CLI reads that metadata together
//! with resolved type information (via rustdoc JSON) to build a fixture
//! dependency graph and generate a [`kitest`](https://docs.rs/kitest) harness.

pub use testrs_macros::{fixture, test};

/// Provides a human-readable name for a test case value.
///
/// Implement this on a `#[test(cases(...))]` provider's element type to control
/// how its cases appear in test output. testrs prefers this over `Debug` /
/// `Display`; if none are implemented it falls back to the case index.
///
/// ```ignore
/// impl testrs::TestCaseName for Vector {
///     fn case_name(&self) -> String { self.id.clone() }
/// }
/// ```
pub trait TestCaseName {
    fn case_name(&self) -> String;
}
