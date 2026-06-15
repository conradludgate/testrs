//! The case system itself: cartesian products, and how cases are named.
//!
//! A `#[test(cases(...))]` test runs once per element of the named providers'
//! cartesian product. Each case is named `test{param=value}`, where the value is
//! rendered by the first of [`TestCaseName`](testrs::TestCaseName), `Debug`, or
//! `Display` the case type implements — otherwise its index. The [`crypto`] and
//! [`sqlite`] modules show the realistic uses; this one isolates each behaviour.
//!
//! [`crypto`]: crate::crypto
//! [`sqlite`]: crate::sqlite

/// Cartesian product: the test runs over every `(x, y)` pair (3 × 2 = 6 cases).
/// `u32` cases have no naming trait of their own but are `Debug`, so they're
/// named by `Debug`: `{x=0,y=7}`, …
pub mod product {
    use testrs::test;

    pub fn xs() -> Vec<u32> {
        vec![0, 1, u32::MAX]
    }

    pub fn ys() -> Vec<u32> {
        vec![0, 7]
    }

    /// `saturating_add` is commutative across the whole grid.
    #[test(cases(x = xs, y = ys))]
    fn saturating_add_is_commutative(x: &u32, y: &u32) {
        assert_eq!(x.saturating_add(*y), y.saturating_add(*x));
    }
}

/// Named by `Display` when the case type implements it but not `TestCaseName`.
pub mod display {
    use std::fmt;
    use testrs::test;

    /// An HTTP status code, displayed as its number.
    pub struct Status(pub u16);

    impl fmt::Display for Status {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{}", self.0)
        }
    }

    pub fn statuses() -> Vec<Status> {
        vec![Status(200), Status(404), Status(500)]
    }

    /// Cases appear as `{s=200}`, `{s=404}`, `{s=500}`.
    #[test(cases(s = statuses))]
    fn status_is_in_range(s: &Status) {
        assert!((100..600).contains(&s.0));
    }
}

/// Falls back to the case index when the type implements none of the naming
/// traits — useful for opaque binary inputs.
pub mod indexed {
    use testrs::test;

    pub struct Blob(pub &'static [u8]);

    pub fn blobs() -> Vec<Blob> {
        vec![Blob(b"\x00\x01\x02"), Blob(b"\xff\xfe")]
    }

    /// Cases appear as `{blob=0}`, `{blob=1}`.
    #[test(cases(blob = blobs))]
    fn blob_is_non_empty(blob: &Blob) {
        assert!(!blob.0.is_empty());
    }
}
