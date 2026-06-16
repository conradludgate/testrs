//! Per-test emission: the `tests.push(...)` block for one test, including its
//! cartesian-product `cases` loops, owned per-test fixtures, and name expression.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use proc_macro2::{Ident, TokenStream};
use quote::{format_ident, quote};

use super::render::{call_tokens, field_ident, fixture_args, type_tokens};
use crate::discover::{CaseNameStrategy, Discovery, ShouldPanic};
use crate::graph::{Graph, Ownership};

/// Build the `tests.push(...)` for one test, wrapped in cartesian-product loops
/// over its `cases` providers (each leaked to `'static`) when it has any.
pub(super) fn emit_test(
    discovery: &Discovery,
    graph: &Graph,
    ti: usize,
    key: &str,
    block_on: &TokenStream,
) -> Result<TokenStream> {
    let item = &discovery.items[ti];
    // Qualify the test name with its module path (nextest's `::` convention) so
    // the group is visible in the output: `users::test_find_user`.
    let name = if item.module_path.is_empty() {
        item.name.clone()
    } else {
        format!("{}::{}", item.module_path.join("::"), item.name)
    };

    // Cases render as `name{param0=value0,param1=value1}`. `{{`/`}}` escape the
    // literal braces in the generated `format!` string.
    let name_expr = if item.cases.is_empty() {
        quote! { #name }
    } else {
        let mut fmt = name.clone();
        fmt.push_str("{{");
        let mut fmt_args: Vec<TokenStream> = Vec::new();
        for (i, case) in item.cases.iter().enumerate() {
            if i > 0 {
                fmt.push(',');
            }
            fmt.push_str(&case.param);
            fmt.push('=');
            let p = format_ident!("{}", case.param);
            match case.name_strategy {
                CaseNameStrategy::Index => {
                    fmt.push_str("{}");
                    let p_i = format_ident!("{}_i", case.param);
                    fmt_args.push(quote! { #p_i });
                }
                CaseNameStrategy::Debug => {
                    fmt.push_str("{:?}");
                    fmt_args.push(quote! { #p });
                }
                CaseNameStrategy::Display => {
                    fmt.push_str("{}");
                    fmt_args.push(quote! { #p });
                }
                CaseNameStrategy::TestCaseName => {
                    fmt.push_str("{}");
                    fmt_args.push(quote! { testrs::TestCaseName::case_name(#p) });
                }
            }
        }
        fmt.push_str("}}");
        quote! { format!(#fmt, #(#fmt_args),*) }
    };

    let owned = emit_owned(discovery, graph, ti, block_on)?;
    let args = test_args(discovery, graph, ti, &owned.locals);
    let call = call_tokens(discovery, ti, &args, block_on);
    // An owned fixture with a `&mut` dep mutates the shared store, so take the
    // store by `&mut` (`with_borrow_mut` hands back a plain `&mut Fixtures`) so
    // disjoint fields can be projected `&mut` and `&` at once; otherwise `&`.
    let with = if owned.needs_mut {
        quote! { with_borrow_mut }
    } else {
        quote! { with_borrow }
    };
    let owned_stmts = &owned.stmts;

    // With `#[skip]`, the closure first calls each generated predicate (with the
    // same arguments as the test); the first that returns `true` yields a `Skipped`
    // marker, carrying its reason, that the harness's `SkipPanicHandler` turns into
    // an ignored result. Otherwise it runs the body. Without skip, the closure just
    // runs the body.
    let closure = if item.skip.is_empty() {
        quote! {
            move || {
                FIXTURES.#with(|c| {
                    #(#owned_stmts)*
                    #call;
                });
            }
        }
    } else {
        let checks: Vec<TokenStream> = item
            .skip
            .iter()
            .map(|skip| {
                let predicate: TokenStream = skip.call.parse().expect("valid skip predicate path");
                let reason = skip.reason.as_deref().unwrap_or("skipped");
                quote! {
                    if #predicate(#(#args),*) {
                        return testrs::harness::skipped(#reason);
                    }
                }
            })
            .collect();
        quote! {
            move || {
                FIXTURES.#with(|c| {
                    #(#owned_stmts)*
                    #(#checks)*
                    #call;
                    testrs::harness::passed()
                })
            }
        }
    };

    // Build the test with the matching `testrs::harness` constructor — keeps the
    // generated `Test`/`TestMeta` plumbing in `testrs` rather than inline here.
    let ctor = match &item.should_panic {
        ShouldPanic::No => quote! {
            testrs::harness::test(#name_expr, #key, #closure)
        },
        ShouldPanic::Any => quote! {
            testrs::harness::test_should_panic(#name_expr, #key, None, #closure)
        },
        ShouldPanic::With(msg) => quote! {
            testrs::harness::test_should_panic(#name_expr, #key, Some(#msg.into()), #closure)
        },
    };

    // No cases: push the single test. With cases: extend with the cartesian
    // product of the providers — `flat_map` for the outer params and `map` for the
    // innermost, so each combination yields one test (first param outermost).
    let body = if item.cases.is_empty() {
        quote! { tests.push(#ctor); }
    } else {
        let mut product: Option<TokenStream> = None;
        for case in item.cases.iter().rev() {
            let p = format_ident!("{}", case.param);
            let p_i = format_ident!("{}_i", case.param);
            let p_cases = format_ident!("{}_cases", case.param);
            product = Some(match product {
                // Innermost provider: each value maps to one test.
                None => quote! { #p_cases.iter().enumerate().map(move |(#p_i, #p)| #ctor) },
                // Outer providers: fan each value out over the inner product.
                Some(inner) => {
                    quote! { #p_cases.iter().enumerate().flat_map(move |(#p_i, #p)| #inner) }
                }
            });
        }
        let product = product.expect("cases is non-empty in this branch");
        quote! { tests.extend(#product); }
    };

    // Leak each provider's collection so the `&T` cases live for the whole run.
    let mut leaks = Vec::new();
    for case in &item.cases {
        let p_cases = format_ident!("{}_cases", case.param);
        let elem = type_tokens(&case.element)?;
        let provider: TokenStream = case.provider_call.parse().expect("valid provider path");
        leaks.push(quote! {
            let #p_cases: &'static [#elem] =
                Vec::leak(#provider().into_iter().collect::<Vec<_>>());
        });
    }

    Ok(quote! { #(#leaks)* #body })
}

/// Argument expressions for invoking a test: case params use their loop
/// variable; fixture params come from the store (borrowed) or an owned local.
fn test_args(
    discovery: &Discovery,
    graph: &Graph,
    ti: usize,
    owned: &HashMap<usize, Ident>,
) -> Vec<TokenStream> {
    let item = &discovery.items[ti];
    let node = &graph.nodes[ti];
    item.sig
        .inputs
        .iter()
        .map(|(param, _ty)| {
            if item.cases.iter().any(|c| &c.param == param) {
                let p = format_ident!("{}", param);
                quote! { #p }
            } else {
                let edge = node
                    .edges
                    .iter()
                    .find(|e| &e.param == param)
                    .expect("fixture edge for non-case parameter");
                match edge.ownership {
                    Ownership::Borrowed => {
                        let f = field_ident(discovery, edge.target);
                        quote! { c.#f.as_ref().unwrap() }
                    }
                    // Tests can't take `&mut` (rejected during graph validation),
                    // so this arm only exists for exhaustiveness.
                    Ownership::BorrowedMut => {
                        let f = field_ident(discovery, edge.target);
                        quote! { c.#f.as_mut().unwrap() }
                    }
                    Ownership::Owned => {
                        let l = &owned[&edge.target];
                        quote! { #l }
                    }
                }
            }
        })
        .collect()
}

/// Accumulator for the per-test owned-fixture build.
#[derive(Default)]
struct OwnedFixtures {
    /// `let <local> = <call>;` statements, in dependency order.
    stmts: Vec<TokenStream>,
    /// Fixture index → its local binding ident.
    locals: HashMap<usize, Ident>,
    /// Fixtures currently on the build stack (cycle guard).
    building: HashSet<usize>,
    /// Whether any built fixture mutably borrows the store.
    needs_mut: bool,
}

/// Build all owned per-test fixtures required (transitively) by `consumer`, in
/// dependency order.
fn emit_owned(
    discovery: &Discovery,
    graph: &Graph,
    consumer: usize,
    block_on: &TokenStream,
) -> Result<OwnedFixtures> {
    let mut owned = OwnedFixtures::default();
    build_owned(discovery, graph, consumer, block_on, &mut owned)?;
    Ok(owned)
}

fn build_owned(
    discovery: &Discovery,
    graph: &Graph,
    consumer: usize,
    block_on: &TokenStream,
    owned: &mut OwnedFixtures,
) -> Result<()> {
    for edge in &graph.nodes[consumer].edges {
        if edge.ownership != Ownership::Owned {
            continue;
        }
        let target = edge.target;
        if owned.locals.contains_key(&target) || !owned.building.insert(target) {
            continue;
        }
        build_owned(discovery, graph, target, block_on, owned)?;
        if graph.nodes[target]
            .edges
            .iter()
            .any(|e| e.ownership == Ownership::BorrowedMut)
        {
            owned.needs_mut = true;
        }
        let args = fixture_args(discovery, &graph.nodes[target], &owned.locals);
        let local = field_ident(discovery, target);
        let call = call_tokens(discovery, target, &args, block_on);
        owned.stmts.push(quote! { let #local = #call; });
        owned.locals.insert(target, local);
    }
    Ok(())
}
