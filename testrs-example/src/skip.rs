//! A test that decides, at run time and from its fixtures, to skip itself.
//!
//! Sometimes a test *can't* run meaningfully through no fault of the code: you
//! provision an object on a remote service and its returned state rules out the
//! scenario you wanted to exercise. Failing would be wrong (nothing is broken) and
//! so would passing (you tested nothing) — so you skip, and the test is reported
//! *ignored*.
//!
//! `#[skip(EXPR, reason = "...")]` expresses exactly that. `EXPR` is a `bool`
//! expression evaluated with the test's fixtures; here it stands in for "the
//! service handed back something we can't test against". This example is contrived
//! on purpose — a `Ticket` fixture carries a random id, and the test skips when it
//! can't be exercised. A test may stack several `#[skip]`s; it skips on the first
//! whose condition holds, so the two below cover even ids and multiples of three.

use testrs::{fixture, skip, test};

/// Stand-in for an object provisioned by some external service: we don't control
/// what it comes back as.
pub struct Ticket {
    pub id: u64,
}

#[fixture]
fn ticket() -> Ticket {
    // Stand-in for a value the service hands back; we can't predict it.
    Ticket {
        id: rand::random::<u64>(),
    }
}

#[test]
#[skip(ticket.id.is_multiple_of(2), reason = "even ticket ids can't be exercised")]
#[skip(ticket.id.is_multiple_of(3), reason = "ids divisible by three are quarantined")]
fn processes_odd_ticket(ticket: &Ticket) {
    // Only reached when both skip conditions above were false.
    assert!(
        !ticket.id.is_multiple_of(2) && !ticket.id.is_multiple_of(3),
        "skip should have spared this id"
    );
}
