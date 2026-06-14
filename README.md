# testrs

> A pytest-inspired Rust test framework with **compile-time, type-based fixture
> dependency injection**.

testrs lets you declare test **fixtures** and **tests** as plain functions and
wires them together **by type**: a test that needs a `&Database` is given the
fixture that produces a `Database`, set up once and shared according to where it
lives in the module tree. A small CLI reads your crate's API (via rustdoc JSON),
resolves the fixture graph, and generates a [kitest](https://docs.rs/kitest)
test harness — no `Arc`, no statics, no name-matching.

> **Status: experimental.** This is a working prototype, not a published crate.
> Expect rough edges and breaking changes. See [Limitations](#limitations).

```rust
use testrs::{fixture, test};

#[fixture]
async fn database(config: &Config) -> Database {
    Database::connect(&config.url).await
}

#[test]
async fn finds_a_user(db: &Database, user: User) {
    assert!(db.find(user.id).await.is_some());
}
```

```console
$ testrs test my-tests
group users, running 1 tests
test users::finds_a_user ... ok
test result: ok. 1 passed; 0 failed; ...
```

---

This README follows the [Diátaxis](https://diataxis.fr/) model:

- **[Tutorial](#tutorial-your-first-fixture-and-test)** — get a suite running from scratch.
- **[How-to guides](#how-to-guides)** — recipes for specific tasks.
- **[Reference](#reference)** — the macros, traits, CLI, and rules.
- **[Explanation](#explanation-how-testrs-works)** — how and why it works.

---

## Tutorial: your first fixture and test

### 1. Prerequisites

- The `testrs` CLI builds on **stable** Rust.
- The crate you analyze is documented with a **pinned nightly** (rustdoc JSON is
  nightly-only). Install the default:

  ```console
  $ rustup toolchain install nightly-2026-04-16
  ```

### 2. Create a test crate

testrs analyzes a crate's **library**. Fixtures and tests are `pub` functions in
that lib, so the conventional setup is a **dedicated crate** for your suite (so
test code doesn't ship in your production library):

```toml
# my-tests/Cargo.toml
[package]
name = "my-tests"
edition = "2024"

[dependencies]
testrs = { git = "https://github.com/conradludgate/testrs" }
```

`testrs` is the *only* dependency you add — the generated harness pulls in
`kitest` and `tokio` itself.

### 3. Write fixtures and tests

```rust
// my-tests/src/lib.rs
#![allow(unknown_or_malformed_diagnostic_attributes)] // required (see Reference)

use testrs::{fixture, test};

pub struct Config { pub url: String }
pub struct Database;

#[fixture]
fn config() -> Config {
    Config { url: "postgres://localhost".into() }
}

#[fixture]
async fn database(config: &Config) -> Database {
    let _ = &config.url;
    Database
}

#[test]
async fn connects(db: &Database) {
    let _ = db;
}
```

`connects` asks for `&Database`; testrs finds the `database` fixture, which in
turn asks for `&Config` and gets `config`. You never wire them up by hand.

### 4. Run it

```console
$ testrs test my-tests
group connects, running 1 tests
test connects ... ok
test result: ok. 1 passed; ...
```

`testrs test` generates an ephemeral harness crate under `target/` and runs it —
your worktree is never touched. (From this repository you can run the bundled
example with `cargo run -p testrs-cli -- test testrs-example --manifest-path
testrs-example/Cargo.toml`.)

---

## How-to guides

### Share a fixture across many tests

Define a fixture where you want it shared — every test in that module (and its
submodules) borrows the same instance. A fixture at the crate root is shared by
the whole suite:

```rust
#[fixture]
async fn database() -> Database {
    Database::connect().await        // built once, not once per test
}

pub mod users {
    use super::Database;
    use testrs::test;

    #[test]
    async fn lists(db: &Database) { assert!(db.users().await.is_empty()); }

    #[test]
    async fn counts(db: &Database) { assert_eq!(db.count().await, 0); }
}
```

`database` is built **once** before the `users` group runs and dropped after both
tests finish. Move it inside `users` to share it only with that module's tests.

### Make a fresh value per test

Ask for the value **by value** (`T`, not `&T`) and define the fixture in the
same module as the test. It's constructed fresh for each test and dropped after:

```rust
#[fixture]
fn user(db: &Database) -> User { db.make_user() }   // borrows shared db

#[test]
async fn deletes_a_user(db: &Database, user: User) {  // owns a fresh user
    db.delete(user.id).await;
}
```

### Use async fixtures and tests

Just write `async fn`. testrs runs everything on a single tokio runtime and
bridges at the boundaries — you don't add `tokio` to your crate.

### Run a test over a table of cases

Point `cases` at a provider function returning a `Vec<T>`; the test runs once per
element, received by reference:

```rust
pub struct Vector { pub input: u32, pub expected: u32 }

pub fn vectors() -> Vec<Vector> {
    parse(include_str!("vectors.txt"))   // runs at collection time
}

#[test(cases(v = vectors))]
fn checks_vector(v: &Vector) {
    assert_eq!(transform(v.input), v.expected);
}
```

### Run over a cartesian product

Name several providers; the test runs over every combination:

```rust
pub fn lefts()  -> Vec<u32> { vec![1, 2] }
pub fn rights() -> Vec<u32> { vec![10, 20] }

#[test(cases(l = lefts, r = rights))]   // 2 × 2 = 4 cases
fn sums(l: &u32, r: &u32) {
    assert!(l + r > *l);
}
// -> sums{l=1,r=10}, sums{l=1,r=20}, sums{l=2,r=10}, sums{l=2,r=20}
```

### Give cases readable names

Each case is named `test{param=value}`. The value is rendered by the first of
these the case type implements: [`TestCaseName`](#the-testcasename-trait),
`Debug`, `Display`, else the index.

```rust
impl testrs::TestCaseName for Vector {
    fn case_name(&self) -> String { format!("rfc_{}", self.input) }
}
// -> checks_vector{v=rfc_2}, checks_vector{v=rfc_3}, ...
```

### Assert that a test panics

```rust
#[test(should_panic)]
fn rejects_empty() { parse("").unwrap(); }

#[test(should_panic = "denominator")]      // panic message must contain this
fn rejects_zero() { divide(1, 0); }
```

### Run under cargo-nextest

```console
$ testrs test my-tests --nextest
```

The harness implements the libtest list/filter protocol, so nextest runs each
test in its own process. Case providers must be **deterministic** so test names
match between nextest's list and run passes.

### Inspect without running

```console
$ testrs discover my-tests   # list discovered fixtures/tests + resolved signatures
$ testrs graph    my-tests   # build & validate the dependency graph, print it
$ testrs generate my-tests   # print the generated harness source (for debugging)
```

---

## Reference

### Macros

| Form | Meaning |
|---|---|
| `#[fixture]` | Marks a function as a fixture. Its return type is what it provides. |
| `#[test]` | Marks a test function. |
| `#[test(cases(p = provider, ...))]` | Data-driven test; runs over the cartesian product of the providers. Each `p` is a `&T` parameter. |
| `#[test(should_panic)]` | The test is expected to panic. |
| `#[test(should_panic = "msg")]` | …and its panic message must contain `"msg"`. |

Both macros leave the function body unchanged and promote it to `pub` (so the
generated harness can call it).

### Parameter ownership

A fixture/test parameter's type decides how the value is supplied:

- **`&T`** — a fixture defined in an **ancestor** (or the same) module, borrowed
  from the shared fixture stack.
- **`T`** — a **per-test** fixture in the same module, constructed fresh and moved
  in.

### Fixture resolution

For a parameter of (underlying) type `T`, testrs walks up the module tree from
the consumer and uses the **closest** `#[fixture]` returning `T`. Two fixtures
producing `T` at the same level is an error.

### The `TestCaseName` trait

```rust
pub trait TestCaseName {
    fn case_name(&self) -> String;
}
```

Implement it on a case (provider element) type to control how its cases are named
in output. testrs prefers it over `Debug`/`Display`.

### Case providers

A `cases` provider is a **plain function** (not marked) that:

- is **`pub`** and reachable by the path you name (`provider`, or `crate::a::provider`),
- returns **`Vec<T>`**,
- is **synchronous** and **self-contained** (it runs at collection time, before
  any fixtures exist).

The case type `T` must be `Sync + 'static`.

### Crate setup requirements

- Add `#![allow(unknown_or_malformed_diagnostic_attributes)]` at the crate root.
  (testrs markers ride in the `diagnostic::testrs` namespace, which rustc warns
  about but otherwise ignores.)
- Modules containing fixtures/tests must be `pub mod`.
- The analyzed crate's only required dependency is `testrs`.
- Have the rustdoc nightly installed (`--toolchain` to change it).

### CLI

```
testrs <command> <PACKAGE> [--manifest-path <PATH>] [--toolchain <NAME>]

  discover   List fixtures/tests with resolved signatures.
  graph      Build & validate the fixture dependency graph.
  generate   Print the generated harness source (stdout).
  test       Generate and run the suite.   [--nextest]
```

Defaults: `--manifest-path Cargo.toml`, `--toolchain nightly-2026-04-16`.

### Validation errors

`graph`/`test` report, with the consumer and parameter:

- **missing** — no in-scope fixture produces the requested type,
- **ambiguous** — two fixtures produce it at the same scope,
- **owns-ancestor** — a `T` parameter resolves to a fixture shared at a broader
  scope (borrow it with `&` instead),
- **cycle** — fixtures depend on each other circularly.

---

## Explanation: how testrs works

### The pipeline

```
#[fixture] / #[test]          testrs-cli                         generated harness
──────────────────────  ──────────────────────────────  ──────────────────────────
emit #[diagnostic::      1. cargo +nightly rustdoc → JSON   ephemeral crate in target/:
  testrs::*] markers     2. resolve signatures by type        - tokio runtime
                         3. build + validate fixture graph     - kitest harness
                         4. generate the harness               - fixture setup/teardown
                                                            run via cargo test / nextest
```

1. The **proc macros** are thin markers. They attach an inert
   `#[diagnostic::testrs::*]` attribute (the one attribute namespace rustdoc
   preserves verbatim) and otherwise leave your function alone.
2. **`testrs-cli`** runs `cargo rustdoc` against your crate (using a pinned
   nightly), reads the JSON with
   [rustdoc-reflection](https://github.com/LukeMathWalker/rustdoc-reflection),
   and resolves each marked function's parameter/return **types**.
3. It builds a **dependency graph** keyed on those types, applies the
   module-tree scoping rules, validates it, and topologically orders the
   fixtures.
4. It generates a [kitest](https://docs.rs/kitest) harness into an **ephemeral
   crate under `target/`** and runs it. The harness uses a single tokio runtime,
   groups tests by leaf module, and keeps an active **scope stack** so each
   shared fixture is built once and torn down when its scope is left.

### Why a CLI instead of macros alone?

Resolving fixtures *by type* needs full type information — which proc macros
don't have, but rustdoc JSON does. This is the same approach
[Pavex](https://github.com/LukeMathWalker/pavex) uses for its dependency
injection. The generated harness is plain, inspectable Rust (`testrs generate`).

### Why an ephemeral crate?

The harness must be a real cargo **test target** (so `cargo test` and `cargo
nextest` can build and drive it), but generating it into your worktree would mean
committing generated code and adding `kitest`/`tokio` dev-dependencies. Instead
it's written under `target/` (gitignored) and regenerated on every run, so it
can never drift and your crate stays clean.

### Toolchain split

`testrs-cli` builds on stable. Only the **rustdoc-JSON generation** for the crate
under test needs a nightly, invoked as a subprocess. The pinned version must emit
a rustdoc-JSON format version matching the `rustdoc-types` testrs uses.

---

## Limitations

Not yet supported (contributions/ideas welcome):

- Generic fixture/case types.
- A shared (ancestor) fixture that takes an owned dependency.
- Case providers that are `async`, use fixtures, or return `impl IntoIterator`.
- Per-case expansion is not yet pruned when nextest runs a single case.
- `#[ignore]`/skip, tags/filtering by tag, parametrize-with-literals, property
  testing, shuffling/sharding/seeds.

## License

Licensed under either of MIT or Apache-2.0 at your option.
