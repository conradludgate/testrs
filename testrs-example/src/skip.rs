//! A test that decides, at run time and from its fixtures, to skip itself.
//!
//! Sometimes a test *can't* run meaningfully through no fault of the code: you
//! provision an object on a remote service and its returned state rules out the
//! scenario you wanted to exercise. Failing would be wrong (nothing is broken) and
//! so would passing (you tested nothing) — so you skip, and the test is reported
//! *ignored*.
//!
//! `#[skip(if = EXPR, reason = "...")]` expresses exactly that. `EXPR` is a `bool`
//! expression evaluated with the test's fixtures; here it stands in for "the
//! service handed back something we can't test against". This example is contrived
//! on purpose — a `Ticket` fixture carries a pseudo-random id, and the test skips
//! when it's even — so roughly half of all runs report this test as ignored.

use testrs::{fixture, skip, test};

/// Stand-in for an object provisioned by some external service: we don't control
/// what it comes back as.
pub struct Ticket {
    pub id: u64,
}

#[fixture]
fn ticket() -> Ticket {
    use std::hash::BuildHasher;
    // A "random enough" id without pulling in a dependency: the std `HashMap` seed
    // is randomized per construction (from the OS), so hashing a constant with a
    // fresh one yields an unpredictable value. Even ids stand in for "can't
    // exercise this one".
    let id = std::collections::hash_map::RandomState::new().hash_one(0u8);
    Ticket { id }
}

#[test]
#[skip(if = ticket.id.is_multiple_of(2), reason = "even ticket ids can't be exercised")]
fn processes_odd_ticket(ticket: &Ticket) {
    // Only reached when the skip condition above was false, so the id is odd.
    assert!(
        !ticket.id.is_multiple_of(2),
        "skip should have spared even ids"
    );
}
