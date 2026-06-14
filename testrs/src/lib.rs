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
    /// Parse the process arguments.
    pub fn from_env() -> Self {
        let mut args = TestArgs {
            list: false,
            ignored: false,
            exact: false,
            filters: Vec::new(),
        };
        let raw: Vec<String> = std::env::args().skip(1).collect();
        let mut i = 0;
        while i < raw.len() {
            match raw[i].as_str() {
                "--list" => args.list = true,
                "--ignored" | "--include-ignored" => args.ignored = true,
                "--exact" => args.exact = true,
                "--nocapture" => {}
                // `--format <value>`: skip the value (we only emit `terse`).
                "--format" => i += 1,
                flag if flag.starts_with('-') => {}
                _ => args.filters.push(raw[i].clone()),
            }
            i += 1;
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
