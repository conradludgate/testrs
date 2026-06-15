//! Worked examples for testrs — each module is a self-contained scenario you
//! could lift into a real test suite, not a toy.
//!
//! These fixtures/tests aren't run by `cargo test`; drive them with the CLI:
//!
//! ```console
//! $ testrs test testrs-example --manifest-path testrs-example/Cargo.toml
//! ```
//!
//! - [`sqlite`] — an in-memory SQLite database, migrated and seeded once and
//!   shared across a module's tests, plus fresh per-test databases for writes.
//! - [`crypto`] — SHA-256 and HMAC-SHA256 test-vector suites: the data-driven
//!   pattern testrs is built for, using real published vectors.
//! - [`cases`] — the case system itself: cartesian products and case naming.
//! - [`panics`] — tests asserted to panic.
//! - [`skip`] — a test that decides at run time, from its fixtures, to skip.
//! - [`async_runtime`] — async fixtures/tests driven through a `#[testrs::runtime]`
//!   bridge.
#![allow(unknown_or_malformed_diagnostic_attributes)]

pub mod async_runtime;
pub mod cases;
pub mod crypto;
pub mod panics;
pub mod skip;
pub mod sqlite;
