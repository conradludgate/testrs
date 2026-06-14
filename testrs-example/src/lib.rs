//! Example crate exercising the testrs marker macros.
//!
//! These fixtures/tests aren't executed by `cargo test` directly — run them
//! with `testrs test testrs-example`.
#![allow(unknown_or_malformed_diagnostic_attributes)]

use testrs::fixture;

pub struct Config {
    pub url: String,
}
pub struct Database;
pub struct User {
    pub id: u64,
}

/// Suite-wide config fixture, shared across every group.
#[fixture]
fn config() -> Config {
    Config {
        url: "postgres://localhost".into(),
    }
}

/// Database fixture, borrowing the ancestor `Config`. Built once and reused by
/// both the `users` and `posts` groups.
#[fixture]
async fn database(config: &Config) -> Database {
    let _ = &config.url;
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

    #[test]
    async fn test_list_users(db: &Database) {
        let _ = db;
    }
}

pub mod posts {
    use super::Database;
    use testrs::test;

    #[test]
    async fn test_create_post(db: &Database) {
        let _ = db;
    }
}
