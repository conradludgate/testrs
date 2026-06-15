//! Testing against a real in-memory SQLite database.
//!
//! A single `:memory:` connection is opened once for the whole module. Two setup
//! fixtures borrow it `&mut`: [`schema`] creates the tables, then [`seed`] — which
//! depends on `&Schema`, so testrs builds it *after* — loads reference rows. All
//! three share that one connection, so every test in [`queries`] sees a fully
//! migrated, seeded database, and the open + migrate + seed cost is paid once.
//!
//! That shared-root "diamond" is something a per-instantiation fixture framework
//! can't express: there, each `&mut` setup step would get its own database. The
//! [`isolated`] submodule shows the opposite policy — a fresh database per test —
//! for tests that write, and [`session`] shows asynchronous teardown.

use rusqlite::Connection;
use testrs::fixture;

/// Schema applied by [`schema`]. Kept beside the fixture that runs it.
const MIGRATE: &str = "
    CREATE TABLE authors (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
    CREATE TABLE posts (
        id     INTEGER PRIMARY KEY,
        author INTEGER NOT NULL REFERENCES authors(id),
        title  TEXT NOT NULL
    );";

/// Reference rows loaded by [`seed`].
const SEED: &str = "
    INSERT INTO authors (id, name) VALUES (1, 'Ada Lovelace'), (2, 'Alan Turing');
    INSERT INTO posts (author, title) VALUES
        (1, 'Notes on the Analytical Engine'),
        (1, 'On Bernoulli Numbers'),
        (2, 'Computing Machinery and Intelligence');";

/// One in-memory database, opened once and shared by the whole module.
#[fixture]
fn db() -> Connection {
    Connection::open_in_memory().expect("open in-memory database")
}

/// Schema migration — borrows the shared connection `&mut` to create the tables.
pub struct Schema;

#[fixture]
fn schema(db: &mut Connection) -> Schema {
    db.execute_batch(MIGRATE).expect("create schema");
    Schema
}

/// Reference data. Depends on `&Schema`, so testrs builds it *after* the schema,
/// reusing the same `&mut` connection.
pub struct Seed;

#[fixture]
fn seed(schema: &Schema, db: &mut Connection) -> Seed {
    let _ = schema;
    db.execute_batch(SEED).expect("seed data");
    Seed
}

/// Read-only tests share the one migrated, seeded connection.
pub mod queries {
    use super::{Connection, Seed};
    use testrs::{TestCaseName, test};

    #[test]
    fn counts_seeded_authors(db: &Connection, _seed: &Seed) {
        let authors: i64 = db
            .query_row("SELECT count(*) FROM authors", [], |row| row.get(0))
            .unwrap();
        assert_eq!(authors, 2);
    }

    #[test]
    fn joins_posts_to_authors(db: &Connection, _seed: &Seed) {
        let author: String = db
            .query_row(
                "SELECT a.name FROM posts p JOIN authors a ON a.id = p.author
                 WHERE p.title = 'On Bernoulli Numbers'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(author, "Ada Lovelace");
    }

    /// One case per (author, expected post count), run against the same shared
    /// connection — `cases` and fixtures combine freely.
    pub struct PostCount {
        pub author: &'static str,
        pub posts: i64,
    }

    impl TestCaseName for PostCount {
        fn case_name(&self) -> String {
            self.author.replace(' ', "_")
        }
    }

    pub fn post_counts() -> Vec<PostCount> {
        vec![
            PostCount {
                author: "Ada Lovelace",
                posts: 2,
            },
            PostCount {
                author: "Alan Turing",
                posts: 1,
            },
        ]
    }

    #[test(cases(case = post_counts))]
    fn author_has_expected_posts(db: &Connection, _seed: &Seed, case: &PostCount) {
        let posts: i64 = db
            .query_row(
                "SELECT count(*) FROM posts p JOIN authors a ON a.id = p.author
                 WHERE a.name = ?1",
                [case.author],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(posts, case.posts);
    }
}

/// The opposite policy: a fresh, migrated database **per test**, so tests can
/// write without affecting each other. Asking for `Connection` *by value* (not
/// `&Connection`) gets a brand-new one each time.
pub mod isolated {
    use super::{Connection, MIGRATE};
    use testrs::{fixture, test};

    #[fixture]
    fn conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory database");
        conn.execute_batch(MIGRATE).expect("create schema");
        conn
    }

    #[test]
    fn insert_is_isolated(conn: Connection) {
        conn.execute("INSERT INTO authors (name) VALUES ('Grace Hopper')", [])
            .unwrap();
        let authors: i64 = conn
            .query_row("SELECT count(*) FROM authors", [], |row| row.get(0))
            .unwrap();
        // A private database: only this test's row, none from a sibling or seed.
        assert_eq!(authors, 1);
    }

    #[test]
    fn starts_empty(conn: Connection) {
        let authors: i64 = conn
            .query_row("SELECT count(*) FROM authors", [], |row| row.get(0))
            .unwrap();
        assert_eq!(authors, 0);
    }
}

/// Teardown. A `#[tear_down]` function takes a shared fixture by value when its
/// scope ends and runs cleanup — sync or `async` (driven through the crate's
/// `#[runtime]` bridge). It works for both, and needs no nightly `AsyncDrop`.
///
/// A teardown'd fixture is shared, so a test borrows it (`&Session`); taking it by
/// value is a compile error. rusqlite is synchronous, so the `await` here is
/// illustrative: a pooled or networked database would await while closing.
pub mod session {
    use rusqlite::Connection;
    use testrs::{fixture, tear_down, test};

    /// A database session, torn down when the module's tests finish.
    pub struct Session {
        conn: Connection,
    }

    #[fixture]
    fn session() -> Session {
        let conn = Connection::open_in_memory().expect("open session database");
        conn.execute_batch("CREATE TABLE events (id INTEGER PRIMARY KEY, kind TEXT NOT NULL);")
            .expect("create schema");
        Session { conn }
    }

    /// Consumes the session and cleans up. Drops the table it created before the
    /// connection closes — a real pooled/networked session would await its close.
    #[tear_down]
    async fn close_session(session: Session) {
        tokio::task::yield_now().await;
        session
            .conn
            .execute_batch("DROP TABLE events;")
            .expect("drop events table");
        let tables: i64 = session
            .conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table'",
                [],
                |row| row.get(0),
            )
            .expect("count remaining tables");
        // On stdout, the same stream kitest reports on, so it isn't lost.
        println!("[session] dropped the events table; {tables} table(s) remain");
    }

    #[test]
    fn records_an_event(session: &Session) {
        session
            .conn
            .execute("INSERT INTO events (kind) VALUES ('login')", [])
            .unwrap();
        let events: i64 = session
            .conn
            .query_row("SELECT count(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(events, 1);
    }
}
