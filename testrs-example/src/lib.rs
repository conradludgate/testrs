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

/// Data-driven tests: one test per parsed vector (like crypto test vectors).
/// `Doubling` implements `TestCaseName`, so cases are named by it.
pub mod vectors {
    use testrs::{TestCaseName, test};

    pub struct Doubling {
        pub input: u32,
        pub doubled: u32,
    }

    impl TestCaseName for Doubling {
        fn case_name(&self) -> String {
            format!("double_{}", self.input)
        }
    }

    /// Parsed at collection time (here inline; in practice from a file).
    pub fn doublings() -> Vec<Doubling> {
        vec![
            Doubling {
                input: 2,
                doubled: 4,
            },
            Doubling {
                input: 3,
                doubled: 6,
            },
            Doubling {
                input: 5,
                doubled: 10,
            },
        ]
    }

    #[test(cases(case = doublings))]
    fn test_doubling(case: &Doubling) {
        assert_eq!(case.input * 2, case.doubled);
    }
}

/// Cases named via `Display` (no `Debug`/`TestCaseName`).
pub mod labelled {
    use std::fmt;
    use testrs::test;

    pub struct Label(pub &'static str);
    impl fmt::Display for Label {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{}", self.0)
        }
    }

    pub fn labels() -> Vec<Label> {
        vec![Label("alpha"), Label("beta")]
    }

    #[test(cases(l = labels))]
    fn test_label(l: &Label) {
        assert!(!l.0.is_empty());
    }
}

/// Tests expected to panic.
pub mod panics {
    use testrs::test;

    fn lookup() -> Option<u32> {
        None
    }

    #[test(should_panic)]
    fn test_unwrap_none() {
        lookup().unwrap();
    }

    #[test(should_panic = "denominator")]
    fn test_div_zero() {
        panic!("the denominator was zero");
    }
}

/// Cases with no naming trait — fall back to the index.
pub mod opaque {
    use testrs::test;

    pub struct Token(pub u32);

    pub fn tokens() -> Vec<Token> {
        vec![Token(7), Token(9)]
    }

    #[test(cases(t = tokens))]
    fn test_token(t: &Token) {
        assert!(t.0 > 0);
    }
}

/// Async on a real runtime. testrs prescribes no runtime; this `#[testrs::runtime]`
/// function tells it how to drive async fixtures/tests — here, on tokio. Without
/// one, the harness uses `testrs::block_on` (pollster), which can't run a tokio
/// timer like the one below. Only one `#[runtime]` is allowed per crate, and it
/// governs every async item.
pub mod async_runtime {
    use std::future::Future;
    use std::sync::OnceLock;
    use std::time::Duration;
    use testrs::test;
    use tokio::runtime::Runtime;

    #[testrs::runtime]
    fn rt<F: Future>(f: F) -> F::Output {
        static RT: OnceLock<Runtime> = OnceLock::new();
        RT.get_or_init(|| Runtime::new().unwrap()).block_on(f)
    }

    #[test]
    async fn sleeps_on_tokio() {
        // Needs a tokio runtime context; proves the `#[runtime]` bridge is used.
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// `&mut` fixtures: setup steps mutate one shared fixture in place. `db` builds
/// an in-memory database once; `users` and `posts` each borrow it `&mut` to add a
/// table during setup. The test then borrows the *same* `Db` and sees both tables
/// — the shared-root diamond that per-fixture-instantiation frameworks can't
/// express (there, each setup step would get its own database).
pub mod mocks {
    use testrs::{fixture, test};

    #[derive(Default)]
    pub struct Db {
        pub tables: Vec<&'static str>,
    }
    pub struct Users;
    pub struct Posts;

    #[fixture]
    fn db() -> Db {
        eprintln!("[mocks] building the in-memory database (once)");
        Db::default()
    }

    #[fixture]
    fn users(db: &mut Db) -> Users {
        db.tables.push("users");
        Users
    }

    #[fixture]
    fn posts(db: &mut Db) -> Posts {
        db.tables.push("posts");
        Posts
    }

    #[test]
    fn both_tables_share_one_db(db: &Db, _u: &Users, _p: &Posts) {
        // One Db: both setup fixtures mutated the same shared instance.
        assert_eq!(db.tables.len(), 2);
        assert!(db.tables.contains(&"users"));
        assert!(db.tables.contains(&"posts"));
    }
}

/// Product cases: the test runs over `lefts` × `rights`.
pub mod product {
    use testrs::test;

    pub fn lefts() -> Vec<u32> {
        vec![1, 2]
    }
    pub fn rights() -> Vec<u32> {
        vec![10, 20]
    }

    #[test(cases(l = lefts, r = rights))]
    fn test_sum(l: &u32, r: &u32) {
        assert!(l + r > *l);
    }
}
