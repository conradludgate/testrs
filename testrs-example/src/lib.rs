//! Example crate exercising the testrs marker macros.
//!
//! These fixtures/tests aren't executed yet — they exist so the `testrs` CLI
//! has a real target to discover markers in and resolve signatures against.
#![allow(unknown_or_malformed_diagnostic_attributes)]

use testrs::fixture;

pub struct Config {
    pub url: String,
}
pub struct Database;
pub struct User {
    pub id: u64,
}

/// Suite-wide config fixture.
#[fixture]
fn config() -> Config {
    Config {
        url: "postgres://localhost".into(),
    }
}

/// Database fixture, borrowing the ancestor `Config`.
#[fixture]
async fn database(config: &Config) -> Database {
    let _ = config;
    Database
}

pub mod users {
    use super::{Database, User};
    use testrs::{fixture, test};

    /// Per-test fixture producing an owned `User`.
    #[fixture]
    fn user(db: &Database) -> User {
        let _ = db;
        User { id: 1 }
    }

    #[test]
    async fn test_find_user(db: &Database, user: User) {
        let _ = (db, user);
    }
}
