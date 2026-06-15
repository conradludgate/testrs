//! A pytest-inspired Rust test framework with compile-time, type-based fixture
//! dependency injection.
//!
//! testrs wires fixtures and tests together **by type**: a test that asks for a
//! `&Database` is given the [`fixture`] that produces a `Database`, set up once
//! and shared according to where it sits in the module tree. The companion
//! `testrs` CLI reads your crate's API (via rustdoc JSON), resolves the fixture
//! graph, and generates a [`kitest`](https://docs.rs/kitest) harness — no
//! `Arc`s, no statics, no name-based matching.
//!
//! This crate is the runtime surface: the [`fixture`], [`test`], [`cases`],
//! [`panics`], [`skip`], [`runtime`](macro@runtime), and
//! [`tear_down`](macro@tear_down) attribute macros, the [`TestCaseName`] trait,
//! and [`TestArgs`] (used by the generated harness). Analysis and code generation
//! live in the `testrs` CLI.
//!
//! # Example
//!
//! ```ignore
//! #![allow(unknown_or_malformed_diagnostic_attributes)]
//! use testrs::{fixture, test};
//!
//! pub struct Config { pub url: String }
//! pub struct Database;
//!
//! #[fixture]
//! fn config() -> Config { Config { url: "postgres://localhost".into() } }
//!
//! #[fixture]
//! async fn database(config: &Config) -> Database {
//!     Database::connect(&config.url).await
//! }
//!
//! #[test]
//! async fn connects(db: &Database) { db.ping().await; }
//! ```
//!
//! Run the suite with the CLI — it generates and runs a harness under `target/`,
//! leaving your worktree untouched:
//!
//! ```console
//! $ testrs test my-tests
//! ```
//!
//! # Markers
//!
//! - [`fixture`] — a function whose return type is the value it provides.
//! - [`test`] — a test. Modifiers are written as sibling attributes:
//!   - [`macro@cases`] — `#[cases(p = expr, ...)]`, data-driven tests (one run per
//!     element of the case expressions' cartesian product).
//!   - [`macro@panics`] — `#[panics]` / `#[panics("msg")]`, expects a panic.
//!   - [`macro@skip`] — `#[skip(expr, reason = "...")]`, skips the test at run time
//!     (reported ignored) when `expr`, evaluated with its fixtures, is `true`. May
//!     be repeated; the test skips on the first condition that holds.
//! - [`macro@runtime`] — *(optional)* names the function that runs async
//!   fixtures/tests to completion. Without one, testrs uses [`block_on`]
//!   (a runtime-agnostic default); mark one to plug in tokio, async-std, etc.
//! - [`macro@tear_down`] — *(optional)* names a function that tears a fixture
//!   down (sync or async) when its scope ends, taking the fixture by value.
//!
//! A parameter's type controls how it's supplied: `&T` borrows a shared fixture
//! from an ancestor (or the same) module; `T` takes ownership of a fresh per-test
//! value. A fixture (not a test) may also take `&mut T` to mutate a shared
//! dependency in place during setup — e.g. a `database` fixture plus `users` /
//! `posts` fixtures that each `&mut`-borrow it to add a table, so the test sees a
//! single database with every table.
//!
//! # Requirements
//!
//! Markers ride in the `diagnostic::testrs` namespace, so a crate using testrs
//! must allow the corresponding lint at its root:
//!
//! ```
//! #![allow(unknown_or_malformed_diagnostic_attributes)]
//! ```
//!
//! See the project README for the full guide.

pub use testrs_macros::{cases, fixture, panics, runtime, skip, tear_down, test};

/// Run a future to completion. This is the runtime-agnostic default the generated
/// harness uses for every async fixture and test when no [`runtime`] provider is
/// marked.
///
/// It drives the future on the current thread with a minimal executor
/// ([`pollster`]) and establishes **no** runtime context — so it handles any
/// async that doesn't need a reactor, but not futures that rely on a specific
/// runtime (tokio timers/IO, `tokio::spawn`, etc.). For those, mark a function
/// with [`macro@runtime`] that bridges through your runtime's own `block_on`:
///
/// ```ignore
/// #[testrs::runtime]
/// fn rt<F: std::future::Future>(f: F) -> F::Output {
///     use std::sync::OnceLock;
///     static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
///     RT.get_or_init(|| tokio::runtime::Runtime::new().unwrap()).block_on(f)
/// }
/// ```
pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
    pollster::block_on(future)
}

/// Provides a human-readable name for a test case value.
///
/// Implement this on a `#[cases(...)]` case's element type to control
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

/// The subset of the libtest CLI a generated harness understands — enough for
/// `cargo test` filtering and `cargo nextest`'s list/run protocol.
pub struct TestArgs {
    /// `--list`: print test names instead of running them.
    pub list: bool,
    /// `--ignored`: with `--list`, restrict to ignored tests.
    pub ignored: bool,
    /// `--exact`: name filters must match the whole test name.
    pub exact: bool,
    /// Positional name filters.
    pub filters: Vec<String>,
}

impl TestArgs {
    /// Parse the process arguments. Unknown flags are ignored, so the harness
    /// tolerates extra libtest-style options it doesn't model.
    pub fn from_env() -> Self {
        use lexopt::prelude::{Long, Value};

        let mut args = TestArgs {
            list: false,
            ignored: false,
            exact: false,
            filters: Vec::new(),
        };
        let mut parser = lexopt::Parser::from_env();
        while let Ok(Some(arg)) = parser.next() {
            match arg {
                Long("list") => args.list = true,
                Long("ignored" | "include-ignored") => args.ignored = true,
                Long("exact") => args.exact = true,
                // `--format <value>`: we only emit `terse`, so just consume it.
                Long("format") => {
                    let _ = parser.value();
                }
                Value(val) => args.filters.push(val.to_string_lossy().into_owned()),
                _ => {}
            }
        }
        args
    }

    /// Whether a test with `name` should run under the current filters.
    pub fn matches(&self, name: &str) -> bool {
        if self.filters.is_empty() {
            return true;
        }
        if self.exact {
            self.filters.iter().any(|f| f == name)
        } else {
            self.filters.iter().any(|f| name.contains(f.as_str()))
        }
    }
}
