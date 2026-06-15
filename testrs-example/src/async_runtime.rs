//! Async fixtures and tests, driven through a `#[testrs::runtime]` bridge.
//!
//! testrs prescribes no runtime. This `#[runtime]` function tells it how to run
//! async fixtures/tests to completion — here, on tokio. Without one, the harness
//! uses `testrs::block_on` (pollster), which can't drive a tokio timer like the
//! `sleep` below. Only one `#[runtime]` is allowed per crate, and it governs
//! every async item in it.

use std::future::Future;
use std::sync::OnceLock;
use std::time::Duration;
use testrs::{fixture, test};
use tokio::runtime::Runtime;

#[testrs::runtime]
fn rt<F: Future>(f: F) -> F::Output {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| Runtime::new().unwrap()).block_on(f)
}

pub struct Client {
    pub id: u64,
}

/// An async fixture — also driven to completion through the `#[runtime]` bridge.
#[fixture]
async fn client() -> Client {
    // Needs a tokio runtime context, proving the fixture runs through `rt`.
    tokio::task::yield_now().await;
    Client { id: 7 }
}

#[test]
async fn queries_over_client(client: Client) {
    // A tokio timer also needs the runtime the `#[runtime]` bridge provides.
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(client.id, 7);
}
