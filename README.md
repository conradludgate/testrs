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
`kitest` itself, and testrs prescribes no async runtime (see
[Use async fixtures and tests](#use-async-fixtures-and-tests)).

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
the whole suite. Log from its body to see it's built only once:

```rust
#[fixture]
fn database() -> Database {
    eprintln!("connecting to the database");   // expensive — do it once
    Database
}

pub mod users {
    use super::Database;
    use testrs::test;

    #[test]
    fn lists(db: &Database) { /* ... */ }
    #[test]
    fn counts(db: &Database) { /* ... */ }
}
```

Both tests borrow the same `Database`, so the setup line prints once:

```console
$ testrs test my-tests
connecting to the database

group users, running 2 tests
test users::counts ... ok
test users::lists ... ok

test result: ok. 2 passed; 0 failed; ...
```

The fixture is built **once** before the `users` group runs and dropped after
both tests finish. Move it inside `users` to share it only with that module's
tests.

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

Just write `async fn`. testrs drives each async fixture/test to completion at the
boundary — it prescribes **no** runtime. By default it uses `testrs::block_on` (a
minimal, runtime-agnostic executor), which is enough for async that doesn't need a
reactor.

For async that needs a real runtime (tokio timers/IO, `tokio::spawn`, …), mark one
function — anywhere in the crate — with `#[testrs::runtime]`, and testrs routes
every async fixture/test through it:

```rust
#[testrs::runtime]
fn rt<F: std::future::Future>(f: F) -> F::Output {
    use std::sync::OnceLock;
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| tokio::runtime::Runtime::new().unwrap()).block_on(f)
}
```

The runtime is *your* dependency, not testrs's — swap in async-std, smol, or a
custom `block_on` the same way. At most one `#[runtime]` per crate.

### Run a test over a table of cases

Add a `#[cases(param = ...)]` attribute alongside `#[test]`. The right-hand side
of a binding is **any expression** that evaluates to an `IntoIterator` whose item
matches the parameter — a `param: &T` runs once per `T`, received by reference.
That can be a function call, an inline array or range, or anything else iterable:

```rust
pub struct Vector { pub input: u32, pub expected: u32 }

pub fn vectors() -> Vec<Vector> {
    parse(include_str!("vectors.txt"))   // runs at collection time
}

#[test]
#[cases(v = vectors())]                  // a provider call …
fn checks_vector(v: &Vector) {
    assert_eq!(transform(v.input), v.expected);
}

#[test]
#[cases(n = [2, 3, 5])]                  // … or an inline expression
fn is_prime(n: &u32) {
    assert!(prime(*n));
}
```

The expression must yield **owned** items of `T` (e.g. `[2, 3, 5]`, not `&ARR`),
and may be any `IntoIterator` — a `Vec`, an array, a range, `impl IntoIterator`,
etc. The element type is taken from the parameter, so the expression is
type-checked against it: a mismatch is a compile error pointing at the `cases`.

### Run over a cartesian product

Name several bindings; the test runs over every combination:

```rust
#[test]
#[cases(l = [1, 2], r = 10..30)]         // 2 × 20 = 40 cases
fn sums(l: &u32, r: &u32) {
    assert!(l + r > *l);
}
// -> sums{l=1,r=10}, sums{l=1,r=11}, …
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
#[test]
#[panics]
fn rejects_empty() { parse("").unwrap(); }

#[test]
#[panics("denominator")]                   // panic message must contain this
fn rejects_zero() { divide(1, 0); }
```

### Skip a test based on its fixtures

Sometimes a test can't run meaningfully through no fault of the code — e.g. an
object provisioned by a service comes back in a state the scenario can't exercise.
Failing would be wrong, so `#[skip]` reports it *ignored* instead. The condition is
a `bool` expression evaluated **at run time** with the test's fixtures (think
pytest's `skipif`):

```rust
#[test]
#[skip(if = ticket.id.is_multiple_of(2), reason = "even ids can't be exercised")]
fn processes_odd_ticket(ticket: &Ticket) { /* only runs when the id is odd */ }
```

`reason` is optional (it defaults to the condition's source text). Because the
condition runs with the fixtures, it can read their private members — it lives in
your crate, not the generated harness. Under `--nextest`, where each test is its
own process, a skipped test still exits cleanly and so shows as passed rather than
ignored (nextest decides "skipped" before running).

### Run under cargo-nextest

```console
$ testrs test my-tests --nextest
```

The harness implements the libtest list/filter protocol, so nextest runs each
test in its own process. Case expressions must be **deterministic** so test names
match between nextest's list and run passes.

### Inspect without running

```console
$ testrs discover my-tests          # list discovered fixtures/tests + resolved signatures
$ testrs graph    my-tests          # build & validate the dependency graph (test → fixtures)
$ testrs graph    my-tests --invert # invert it (fixture → what depends on it)
$ testrs generate my-tests          # print the generated harness source (for debugging)
```

---

## Reference

### Macros

| Form | Meaning |
|---|---|
| `#[fixture]` | Marks a function as a fixture. Its return type is what it provides. |
| `#[test]` | Marks a test function. |
| `#[cases(p = expr, ...)]` | *(on a test)* Data-driven test; runs over the cartesian product of the bindings. Each `expr` is an `IntoIterator`, and each `p` is a `&T` parameter. |
| `#[panics]` | *(on a test)* The test is expected to panic. |
| `#[panics("msg")]` | …and its panic message must contain `"msg"`. |
| `#[skip(if = expr, reason = "...")]` | *(on a test)* Skip at run time (reported *ignored*) when `expr`, evaluated with the test's fixtures, is `true`. `reason` is optional. |

`#[cases]`, `#[panics]`, and `#[skip]` are sibling attributes written next to
`#[test]`. The fixture/test macros leave the function body unchanged and promote it
to `pub` (so the generated harness can call it).

### Parameter ownership

A fixture/test parameter's type decides how the value is supplied:

- **`&T`** — a fixture defined in an **ancestor** (or the same) module, borrowed
  from the shared fixture stack.
- **`&mut T`** — *(fixtures only)* an **exclusive** borrow of a shared fixture, to
  mutate it in place during setup. Because shared fixtures are built one at a time,
  no two `&mut` borrows are ever live at once. Tests may not take `&mut` (their
  writes would leak into later tests). A consumer can't borrow the same fixture
  both `&mut` and another way.
- **`T`** — a **per-test** fixture in the same module, constructed fresh and moved
  in.

This is what lets a `database` fixture be set up by several sibling fixtures —
each taking `&mut Database` to add a table — so the test borrowing `&Database`
sees one instance with every table, rather than a separate database per setup step.

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

Implement it on a case (element) type to control how its cases are named in
output. testrs prefers it over `Debug`/`Display`.

### Case expressions

The `expr` in `cases(p = expr)` is evaluated **at collection time** (before any
fixtures exist), so it must be **self-contained**. It can be any expression in
scope — an inline array/range, a `Vec`, or a function call — that yields an
`IntoIterator` of **owned** `T`, where the test parameter is `p: &T`. Behind the
scenes testrs wraps it in a generated `fn() -> impl IntoIterator<Item = T>`, which
is what type-checks `expr` against the parameter and lets the harness collect it.

The case type `T` must be `Sync + 'static`. For determinism under nextest, the
expression must produce the same sequence on every run.

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
  graph      Build & validate the fixture dependency graph.   [--invert]
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
- **mut-in-test** — a test parameter takes `&mut` (only fixtures may),
- **mut-alias** — a consumer borrows one fixture both `&mut` and another way,
- **cycle** — fixtures depend on each other circularly.

---

## Explanation: how testrs works

### The pipeline

```
#[fixture] / #[test]          testrs-cli                         generated harness
──────────────────────  ──────────────────────────────  ──────────────────────────
emit #[diagnostic::      1. cargo +nightly rustdoc → JSON   ephemeral crate in target/:
  testrs::*] markers     2. resolve signatures by type        - kitest harness
                         3. build + validate fixture graph     - fixture setup/teardown
                         4. generate the harness               - async via #[runtime]
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
   crate under `target/`** and runs it. The harness is runtime-agnostic — it
   drives async through the crate's `#[runtime]` bridge (or `testrs::block_on` by
   default) — groups tests by leaf module, and keeps an active **scope stack** so
   each shared fixture is built once and torn down when its scope is left.

### Why a CLI instead of macros alone?

Resolving fixtures *by type* needs full type information — which proc macros
don't have, but rustdoc JSON does. This is the same approach
[Pavex](https://github.com/LukeMathWalker/pavex) uses for its dependency
injection. The generated harness is plain, inspectable Rust (`testrs generate`).

### Why an ephemeral crate?

The harness must be a real cargo **test target** (so `cargo test` and `cargo
nextest` can build and drive it), but generating it into your worktree would mean
committing generated code and adding a `kitest` dev-dependency. Instead
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
- Case expressions that are `async` or use fixtures (they run at collection time,
  before any fixtures exist).
- Per-case expansion is not yet pruned when nextest runs a single case.
- Static `#[ignore]` (unconditional skip; the runtime `#[skip(if = ...)]` is
  supported), tags/filtering by tag, property testing, shuffling/sharding/seeds.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  <http://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
