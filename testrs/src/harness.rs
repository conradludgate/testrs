//! Runtime support for the CLI-generated kitest harness.
//!
//! These items are used **only** by the harness the `testrs` CLI generates under
//! `target/` — never by the crate under test. Hosting them here (rather than
//! emitting them into the harness) keeps that machine-written source small and
//! lets the logic be type-checked and unit-tested.

use std::borrow::Cow;

use kitest::Whatever;
use kitest::outcome::{TestFailure, TestStatus};
use kitest::panic::{PanicExpectation, TestPanicHandler};
use kitest::test::{TestMeta, TestResult};

/// Marker value a `#[skip]`ped test returns; [`SkipPanicHandler`] turns it into
/// [`TestStatus::Ignored`], carrying the reason.
#[derive(Debug, Clone, PartialEq)]
pub struct Skipped(pub Cow<'static, str>);

impl std::fmt::Display for Skipped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The [`TestResult`] a skipped test returns so the harness reports it ignored with
/// `reason`. The generated harness calls this when a `#[skip]` condition holds.
pub fn skipped(reason: impl Into<Cow<'static, str>>) -> TestResult {
    TestResult(Ok(Some(Whatever::from(Skipped(reason.into())))))
}

/// The length of the common prefix of two slices — used by the harness's scope
/// stack to decide which fixture scopes to keep vs. tear down between groups.
pub fn common_prefix<T: PartialEq>(a: &[T], b: &[T]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

/// A [`TestPanicHandler`] that reports a test which returned the [`Skipped`] marker
/// as [`TestStatus::Ignored`]. For every other outcome it matches kitest's
/// `DefaultPanicHandler` (which would otherwise turn the marker into
/// [`TestStatus::Other`]), so tests without `#[skip]` behave exactly as before.
pub struct SkipPanicHandler;

impl<E> TestPanicHandler<E> for SkipPanicHandler {
    fn handle<F: FnOnce() -> TestResult>(&self, f: F, meta: &TestMeta<E>) -> TestStatus {
        use std::panic::{AssertUnwindSafe, catch_unwind};

        let result = catch_unwind(AssertUnwindSafe(f));
        // A test that returned our skip marker is reported ignored. `Whatever`'s
        // `as_any_ref` erases at the box rather than the stored value, so recover
        // the concrete type through `into_any` (on a clone, leaving `result` for
        // the default path below).
        if let Ok(TestResult(Ok(Some(details)))) = &result
            && let Ok(skip) = details.clone().into_any().downcast::<Skipped>()
        {
            return TestStatus::Ignored {
                reason: Some(skip.0),
            };
        }
        TestStatus::Failed(match (result, &meta.should_panic) {
            (Ok(result), PanicExpectation::ShouldNotPanic) => return result.into(),
            (Ok(_), PanicExpectation::ShouldPanic) => TestFailure::DidNotPanic { expected: None },
            (Ok(_), PanicExpectation::ShouldPanicWithExpected(expected)) => {
                TestFailure::DidNotPanic {
                    expected: Some(expected.to_string()),
                }
            }
            (Err(err), PanicExpectation::ShouldNotPanic) => TestFailure::Panicked(payload(err)),
            (Err(_), PanicExpectation::ShouldPanic) => return TestStatus::Passed,
            (Err(err), PanicExpectation::ShouldPanicWithExpected(expected)) => {
                let msg = payload(err);
                if msg.contains(expected.as_ref()) {
                    return TestStatus::Passed;
                }
                TestFailure::PanicMismatch {
                    got: msg,
                    expected: Some(expected.to_string()),
                }
            }
        })
    }
}

/// Convert a panic payload to a string (the common `panic!` payload types).
fn payload(err: Box<dyn std::any::Any + Send + 'static>) -> String {
    err.downcast::<&'static str>()
        .map(|s| s.to_string())
        .or_else(|err| err.downcast::<String>().map(|s| *s))
        .unwrap_or_else(|_| String::from("Box<dyn Any>"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(should_panic: PanicExpectation) -> TestMeta<()> {
        TestMeta {
            name: "t".into(),
            extra: (),
            should_panic,
            ..Default::default()
        }
    }

    #[test]
    fn skip_marker_becomes_ignored_with_reason() {
        let status = SkipPanicHandler.handle(
            || skipped("not applicable"),
            &meta(PanicExpectation::ShouldNotPanic),
        );
        assert_eq!(
            status,
            TestStatus::Ignored {
                reason: Some("not applicable".into())
            }
        );
    }

    #[test]
    fn normal_return_passes() {
        let status = SkipPanicHandler.handle(
            || TestResult(Ok(None)),
            &meta(PanicExpectation::ShouldNotPanic),
        );
        assert_eq!(status, TestStatus::Passed);
    }

    #[test]
    fn panic_fails_and_captures_message() {
        let status =
            SkipPanicHandler.handle(|| panic!("boom"), &meta(PanicExpectation::ShouldNotPanic));
        assert_eq!(
            status,
            TestStatus::Failed(TestFailure::Panicked("boom".into()))
        );
    }

    #[test]
    fn expected_panic_passes_only_on_match() {
        let hit = SkipPanicHandler.handle(
            || panic!("denominator was zero"),
            &meta(PanicExpectation::ShouldPanicWithExpected(
                "denominator".into(),
            )),
        );
        assert_eq!(hit, TestStatus::Passed);

        let miss = SkipPanicHandler.handle(
            || panic!("something else"),
            &meta(PanicExpectation::ShouldPanicWithExpected(
                "denominator".into(),
            )),
        );
        assert!(matches!(
            miss,
            TestStatus::Failed(TestFailure::PanicMismatch { .. })
        ));
    }

    #[test]
    fn common_prefix_counts_shared_leading_elements() {
        assert_eq!(common_prefix(&["a", "b", "c"], &["a", "b", "d"]), 2);
        assert_eq!(common_prefix::<&str>(&[], &["a"]), 0);
    }
}
