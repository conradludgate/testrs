//! The case system itself: case sources, cartesian products, and case naming.
//!
//! The right-hand side of a `cases(param = ...)` binding is any expression that
//! evaluates to an [`IntoIterator`] whose item matches the parameter (`param: &T`
//! ⇒ items of `T`) — an inline array or range, a `Vec`, or a function returning
//! either. The test runs once per element of the named bindings' cartesian
//! product. Each case is named `test{param=value}`, where the value is rendered by
//! the first of [`TestCaseName`](testrs::TestCaseName), `Debug`, or `Display` the
//! case type implements — otherwise its index. The [`crypto`] and [`sqlite`]
//! modules show realistic uses; this one isolates each behaviour.
//!
//! [`crypto`]: crate::crypto
//! [`sqlite`]: crate::sqlite

/// Cartesian product over **inline** sources — a range and an array, written
/// right in the attribute (3 × 2 = 6 cases). `u32` has no naming trait but is
/// `Debug`, so cases are named by `Debug`: `{x=0,y=10}`, …
pub mod product {
    use testrs::{cases, test};

    /// `saturating_add` is commutative across the whole grid.
    #[test]
    #[cases(x = 0..3, y = [10, 20])]
    fn saturating_add_is_commutative(x: &u32, y: &u32) {
        assert_eq!(x.saturating_add(*y), y.saturating_add(*x));
    }
}

/// Named by `Display` when the case type implements it but not `TestCaseName`.
/// The cases are an inline array of values.
pub mod display {
    use std::fmt;
    use testrs::{cases, test};

    /// An HTTP status code, displayed as its number.
    pub struct Status(pub u16);

    impl fmt::Display for Status {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{}", self.0)
        }
    }

    /// Cases appear as `{s=200}`, `{s=404}`, `{s=500}`.
    #[test]
    #[cases(s = [Status(200), Status(404), Status(500)])]
    fn status_is_in_range(s: &Status) {
        assert!((100..600).contains(&s.0));
    }
}

/// Falls back to the case index when the type implements none of the naming
/// traits. The source is a function returning `impl IntoIterator` — providers
/// needn't return a `Vec`.
pub mod indexed {
    use testrs::{cases, test};

    pub struct Blob(pub &'static [u8]);

    pub fn blobs() -> impl IntoIterator<Item = Blob> {
        [Blob(b"\x00\x01\x02"), Blob(b"\xff\xfe")]
    }

    /// Cases appear as `{blob=0}`, `{blob=1}`.
    #[test]
    #[cases(blob = blobs())]
    fn blob_is_non_empty(blob: &Blob) {
        assert!(!blob.0.is_empty());
    }
}
