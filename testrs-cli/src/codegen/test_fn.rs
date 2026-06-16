//! Per-test emission: the block that adds one test to `tests`, including its
//! cartesian-product `cases` iterator, owned per-test fixtures, and name expression.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use proc_macro2::{Ident, TokenStream};
use quote::{format_ident, quote};

use super::render::{call_tokens, field_ident, fixture_args, type_tokens};
use crate::discover::{CaseNameStrategy, Discovery, ShouldPanic};
use crate::graph::{Graph, Ownership};

/// Build the block that adds one test to `tests`, expanded over the cartesian
/// product of its `cases` providers when it has any.
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
                    fmt_args.push(quote! { testrs::TestCaseName::case_name(&#p) });
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

    // Each case test owns its value(s) so the boxed test closure stays `'static`
    // without leaking. With one param, the value is moved out of the collection
    // (`into_iter`). With several, the cartesian product reuses each value across
    // combinations, so the innermost closure clones the values it needs — hence
    // multi-param case types must be `Clone`.
    let body = if item.cases.is_empty() {
        quote! { tests.push(#ctor); }
    } else if item.cases.len() == 1 {
        let case = &item.cases[0];
        let p = format_ident!("{}", case.param);
        let p_i = format_ident!("{}_i", case.param);
        let p_cases = format_ident!("{}_cases", case.param);
        quote! {
            tests.extend(#p_cases.into_iter().enumerate().map(move |(#p_i, #p)| #ctor));
        }
    } else {
        // Clone every param into an owned binding at the innermost level (the only
        // place all of them are in scope), so `#ctor` sees owned values uniformly.
        let clones: Vec<TokenStream> = item
            .cases
            .iter()
            .map(|case| {
                let p = format_ident!("{}", case.param);
                quote! { let #p = #p.clone(); }
            })
            .collect();
        let mut product: Option<TokenStream> = None;
        for case in item.cases.iter().rev() {
            let p = format_ident!("{}", case.param);
            let p_i = format_ident!("{}_i", case.param);
            let p_cases = format_ident!("{}_cases", case.param);
            product = Some(match product {
                // Innermost provider: each value (with the captured outer ones) maps
                // to one test.
                None => quote! {
                    #p_cases.iter().enumerate().map(move |(#p_i, #p)| {
                        #(#clones)*
                        #ctor
                    })
                },
                // Outer providers: borrow the collection and fan each value out over
                // the inner product (a non-`move` closure, so the inner iterator
                // borrows the collection from this scope, not from the closure).
                Some(inner) => quote! {
                    #p_cases.iter().enumerate().flat_map(|(#p_i, #p)| #inner)
                },
            });
        }
        let product = product.expect("cases is non-empty in this branch");
        quote! { tests.extend(#product); }
    };

    // Collect each provider into an owned `Vec` the case values are taken from.
    let mut collect = Vec::new();
    for case in &item.cases {
        let p_cases = format_ident!("{}_cases", case.param);
        let elem = type_tokens(&case.element)?;
        let provider: TokenStream = case.provider_call.parse().expect("valid provider path");
        collect.push(quote! {
            let #p_cases: Vec<#elem> = #provider().into_iter().collect();
        });
    }

    Ok(quote! { #(#collect)* #body })
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
                // Case values are owned by the closure; the test takes `&T`.
                let p = format_ident!("{}", param);
                quote! { &#p }
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
