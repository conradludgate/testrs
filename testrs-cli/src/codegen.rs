//! Generate the kitest harness source from a resolved fixture graph.
//!
//! Shape (validated by hand first): one tokio runtime; `SimpleRunner`
//! (single-threaded, in order); tests grouped by leaf module. Shared (borrowed)
//! fixtures live in one thread-local store and are scoped by the module that
//! defines them. The group runner keeps an *active scope stack*: entering a
//! group sets up the shared fixtures it needs (lazily, once), and a scope's
//! fixtures are torn down only when the runner leaves that scope — so a common
//! ancestor scope is built once and reused across the groups beneath it. Each
//! test closure pulls borrowed fixtures from the store, builds its owned
//! per-test fixtures fresh, and runs the async body via `Handle::block_on`.
//!
//! The harness is built as a `proc_macro2::TokenStream` with `quote!` and
//! formatted with `prettyplease`, so there's no hand-maintained indentation.

use std::collections::{BTreeSet, HashMap, HashSet};

use anyhow::{Result, anyhow, bail};
use proc_macro2::{Ident, TokenStream};
use quote::{format_ident, quote};
use rustdoc_ir::Type;

use crate::discover::{CaseNameStrategy, Discovery, MarkerKind, ShouldPanic};
use crate::graph::{Graph, Node, Ownership};

/// Render a `rustdoc_ir::Type` as Rust source. Paths already carry the crate
/// name as their first segment, so they're valid from the harness target.
fn render_type(ty: &Type) -> Result<String> {
    match ty {
        Type::Path(p) => {
            if !p.generic_arguments.is_empty() {
                bail!("generic types in fixture/test signatures are not supported yet");
            }
            Ok(p.base_type.join("::"))
        }
        Type::Reference(r) => Ok(format!(
            "{}{}",
            if r.is_mutable { "&mut " } else { "&" },
            render_type(&r.inner)?
        )),
        Type::ScalarPrimitive(p) => Ok(p.as_str().to_string()),
        Type::Tuple(t) => {
            let elems: Result<Vec<String>> = t.elements.iter().map(render_type).collect();
            let elems = elems?;
            // Trailing comma keeps a 1-tuple a tuple.
            let trailing = if elems.len() == 1 { "," } else { "" };
            Ok(format!("({}{trailing})", elems.join(", ")))
        }
        other => bail!("unsupported type in a fixture/test signature: {other:?}"),
    }
}

/// A resolved type rendered as tokens, for interpolation.
fn type_tokens(ty: &Type) -> Result<TokenStream> {
    Ok(render_type(ty)?
        .parse()
        .expect("render_type produces valid tokens"))
}

/// `crate::path::to::name` (as tokens) for a discovered item, from the harness.
fn path_tokens(discovery: &Discovery, idx: usize) -> TokenStream {
    let item = &discovery.items[idx];
    let mut segments = vec![discovery.crate_name.clone()];
    segments.extend(item.module_path.iter().cloned());
    segments.push(item.name.clone());
    segments.join("::").parse().expect("valid item path")
}

/// Scope/group key for a module path: `crate` at the root, else the joined path.
fn scope_key(module_path: &[String]) -> String {
    if module_path.is_empty() {
        "crate".to_string()
    } else {
        module_path.join("::")
    }
}

/// The store field identifier for a shared fixture.
fn field_ident(discovery: &Discovery, idx: usize) -> Ident {
    format_ident!("{}", discovery.items[idx].name)
}

/// `<handle>.block_on(<path>(<args>))` for async items, else the bare call.
fn call_tokens(
    discovery: &Discovery,
    idx: usize,
    args: &[TokenStream],
    handle: &TokenStream,
) -> TokenStream {
    let path = path_tokens(discovery, idx);
    let call = quote! { #path(#(#args),*) };
    if discovery.items[idx].sig.is_async {
        quote! { #handle.block_on(#call) }
    } else {
        call
    }
}

/// Argument expressions for a node's fixture dependencies: borrowed deps read
/// from the store (`c.<field>`), owned deps from previously-built locals.
fn fixture_args(
    discovery: &Discovery,
    node: &Node,
    owned: &HashMap<usize, Ident>,
) -> Vec<TokenStream> {
    node.edges
        .iter()
        .map(|edge| match edge.ownership {
            Ownership::Borrowed => {
                let f = field_ident(discovery, edge.target);
                quote! { c.#f.as_ref().unwrap() }
            }
            Ownership::Owned => {
                let l = &owned[&edge.target];
                quote! { #l }
            }
        })
        .collect()
}

pub fn generate(discovery: &Discovery, graph: &Graph) -> Result<String> {
    if !graph.errors.is_empty() {
        bail!("cannot generate a harness: the fixture graph has unresolved errors");
    }

    // A fixture is "shared" (lives in the store) if anything borrows it.
    let mut shared: HashSet<usize> = HashSet::new();
    for node in &graph.nodes {
        for edge in &node.edges {
            if edge.ownership == Ownership::Borrowed {
                shared.insert(edge.target);
            }
        }
    }

    // Tests grouped by leaf module; each distinct module path is a group.
    let mut groups: std::collections::BTreeMap<Vec<String>, Vec<usize>> =
        std::collections::BTreeMap::new();
    for (i, item) in discovery.items.iter().enumerate() {
        if item.kind == MarkerKind::Test {
            groups.entry(item.module_path.clone()).or_default().push(i);
        }
    }

    // Shared fixtures each group needs.
    let group_shared: Vec<(Vec<String>, Vec<usize>, Vec<usize>)> = groups
        .iter()
        .map(|(module_path, tests)| {
            let reachable = reachable(graph, tests);
            let needed: Vec<usize> = graph
                .fixture_order
                .iter()
                .copied()
                .filter(|i| shared.contains(i) && reachable.contains(i))
                .collect();
            (module_path.clone(), tests.clone(), needed)
        })
        .collect();

    // Shared fixtures used by any group, in setup (topo) order.
    let store_fixtures: Vec<usize> = graph
        .fixture_order
        .iter()
        .copied()
        .filter(|i| shared.contains(i) && group_shared.iter().any(|(_, _, s)| s.contains(i)))
        .collect();

    // Store struct fields.
    let mut fields = Vec::new();
    for &fi in &store_fixtures {
        let name = field_ident(discovery, fi);
        let ty = type_tokens(discovery.items[fi].sig.output.as_ref().unwrap())?;
        fields.push(quote! { #name: Option<#ty>, });
    }

    // `ensure_<fixture>` builds a shared fixture once, after its borrowed deps.
    let mut ensure_methods = Vec::new();
    for &fi in &store_fixtures {
        let node = &graph.nodes[fi];
        if node.edges.iter().any(|e| e.ownership == Ownership::Owned) {
            bail!("owned dependency of a shared fixture is not supported yet");
        }
        let name = field_ident(discovery, fi);
        let ensure = format_ident!("ensure_{}", discovery.items[fi].name);
        let dep_ensures: Vec<TokenStream> = node
            .edges
            .iter()
            .map(|e| {
                let d = format_ident!("ensure_{}", discovery.items[e.target].name);
                quote! { self.#d(); }
            })
            .collect();
        let handle = quote! { self.handle };
        let build = if node.edges.is_empty() {
            let call = call_tokens(discovery, fi, &[], &handle);
            quote! { let value = #call; }
        } else {
            let args = fixture_args(discovery, node, &HashMap::new());
            let call = call_tokens(discovery, fi, &args, &handle);
            quote! {
                let value = FIXTURES.with(|c| {
                    let c = c.borrow();
                    #call
                });
            }
        };
        ensure_methods.push(quote! {
            fn #ensure(&self) {
                if FIXTURES.with(|c| c.borrow().#name.is_some()) {
                    return;
                }
                #(#dep_ensures)*
                #build
                FIXTURES.with(|c| c.borrow_mut().#name = Some(value));
            }
        });
    }

    // `ensure(group)` arms: build every shared fixture the group needs.
    let ensure_arms: Vec<TokenStream> = group_shared
        .iter()
        .map(|(module_path, _, needed)| {
            let key = scope_key(module_path);
            let calls: Vec<TokenStream> = needed
                .iter()
                .map(|&fi| {
                    let e = format_ident!("ensure_{}", discovery.items[fi].name);
                    quote! { self.#e(); }
                })
                .collect();
            quote! { #key => { #(#calls)* } }
        })
        .collect();

    // `teardown_scope` arms: drop a scope's fixtures (reverse topo) on leaving it.
    let scopes: BTreeSet<String> = store_fixtures
        .iter()
        .map(|&i| scope_key(&discovery.items[i].module_path))
        .collect();
    let teardown_arms: Vec<TokenStream> = scopes
        .iter()
        .map(|scope| {
            let nones: Vec<TokenStream> = store_fixtures
                .iter()
                .rev()
                .filter(|&&fi| scope_key(&discovery.items[fi].module_path) == *scope)
                .map(|&fi| {
                    let f = field_ident(discovery, fi);
                    quote! { c.#f = None; }
                })
                .collect();
            quote! { #scope => { #(#nones)* } }
        })
        .collect();

    // Each group's ancestor scope chain (kept active even when unused, so a
    // shared scope isn't torn down by a sibling that doesn't need it).
    let target_arms: Vec<TokenStream> = group_shared
        .iter()
        .map(|(module_path, _, _)| {
            let key = scope_key(module_path);
            let chain = ancestor_scopes(discovery, &store_fixtures, module_path);
            quote! { #key => &[#(#chain),*], }
        })
        .collect();

    // One `tests.push(...)` block per test (wrapped in product loops for cases).
    let mut test_blocks = Vec::new();
    for (module_path, tests, _) in &group_shared {
        let key = scope_key(module_path);
        for &ti in tests {
            test_blocks.push(emit_test(discovery, graph, ti, &key)?);
        }
    }

    let file = quote! {
        #![allow(unknown_or_malformed_diagnostic_attributes)]
        #![allow(unused)]

        use std::cell::RefCell;
        use std::ops::ControlFlow;

        use kitest::group::{TestGroupOutcomes, TestGroupRunner};
        use kitest::prelude::*;
        use kitest::runner::SimpleRunner;
        use tokio::runtime::Handle;

        #[derive(Default)]
        struct Fixtures {
            #(#fields)*
        }

        thread_local! {
            static FIXTURES: RefCell<Fixtures> = RefCell::new(Fixtures::default());
        }

        struct FixtureRunner {
            handle: Handle,
            active: RefCell<Vec<&'static str>>,
        }

        impl FixtureRunner {
            #(#ensure_methods)*

            fn ensure(&self, group: &str) {
                match group {
                    #(#ensure_arms)*
                    _ => {}
                }
            }
        }

        fn teardown_scope(scope: &str) {
            FIXTURES.with(|c| {
                let mut c = c.borrow_mut();
                match scope {
                    #(#teardown_arms)*
                    _ => {}
                }
            });
        }

        fn common_prefix(a: &[&str], b: &[&str]) -> usize {
            a.iter().zip(b).take_while(|(x, y)| x == y).count()
        }

        impl<'t> TestGroupRunner<'t, &'static str, &'static str, ()> for FixtureRunner {
            fn run_group<F>(
                &self,
                f: F,
                key: &&'static str,
                _ctx: Option<&()>,
            ) -> ControlFlow<TestGroupOutcomes<'t>, TestGroupOutcomes<'t>>
            where
                F: FnOnce() -> TestGroupOutcomes<'t>,
            {
                let target: &[&'static str] = match *key {
                    #(#target_arms)*
                    _ => &[],
                };
                {
                    let mut active = self.active.borrow_mut();
                    let common = common_prefix(&active, target);
                    for scope in active[common..].iter().rev() {
                        teardown_scope(scope);
                    }
                    active.truncate(common);
                    active.extend_from_slice(&target[common..]);
                }
                self.ensure(*key);
                ControlFlow::Continue(f())
            }
        }

        fn tests(handle: Handle) -> Vec<Test<&'static str>> {
            let mut tests = Vec::new();
            #(#test_blocks)*
            tests
        }

        fn main() -> std::process::ExitCode {
            let rt = tokio::runtime::Runtime::new().unwrap();
            let handle = rt.handle().clone();
            let all = tests(handle.clone());
            let args = testrs::TestArgs::from_env();
            if args.list {
                if !args.ignored {
                    for t in &all {
                        println!("{}: test", t.meta.name);
                    }
                }
                return std::process::ExitCode::SUCCESS;
            }
            let selected: Vec<_> =
                all.into_iter().filter(|t| args.matches(&t.meta.name)).collect();
            kitest::harness(&selected)
                .with_grouper(|m: &TestMeta<&'static str>| m.extra)
                .with_groups(kitest::group::TestGroupBTreeMap::new())
                .with_group_runner(FixtureRunner {
                    handle,
                    active: RefCell::new(Vec::new()),
                })
                .with_runner(SimpleRunner::default())
                .run()
                .exit_code()
        }
    };

    let parsed: syn::File =
        syn::parse2(file).map_err(|e| anyhow!("generated harness is not valid Rust: {e}"))?;
    let mut source = String::from("// @generated by testrs — do not edit.\n");
    source.push_str(&prettyplease::unparse(&parsed));
    Ok(source)
}

/// Build the `tests.push(...)` for one test, wrapped in cartesian-product loops
/// over its `cases` providers (each leaked to `'static`) when it has any.
fn emit_test(discovery: &Discovery, graph: &Graph, ti: usize, key: &str) -> Result<TokenStream> {
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
        quote! { #name.into() }
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
        quote! { format!(#fmt, #(#fmt_args),*).into() }
    };

    let (owned_stmts, owned) = emit_owned(discovery, graph, ti)?;
    let args = test_args(discovery, graph, ti, &owned);
    let handle = quote! { handle };
    let call = call_tokens(discovery, ti, &args, &handle);

    let should_panic = match &item.should_panic {
        ShouldPanic::No => quote! {},
        ShouldPanic::Any => quote! { should_panic: PanicExpectation::ShouldPanic, },
        ShouldPanic::With(msg) => {
            quote! { should_panic: PanicExpectation::ShouldPanicWithExpected(#msg.into()), }
        }
    };

    let mut body = quote! {
        tests.push(Test::new(
            TestFnHandle::from_boxed({
                let handle = handle.clone();
                move || {
                    FIXTURES.with(|c| {
                        let c = c.borrow();
                        #(#owned_stmts)*
                        #call;
                    });
                }
            }),
            TestMeta {
                name: #name_expr,
                extra: #key,
                #should_panic
                ..Default::default()
            },
        ));
    };

    // Wrap in one loop per case provider (innermost first).
    for case in item.cases.iter().rev() {
        let p = format_ident!("{}", case.param);
        let p_i = format_ident!("{}_i", case.param);
        let p_cases = format_ident!("{}_cases", case.param);
        body = quote! {
            for (#p_i, #p) in #p_cases.iter().enumerate() {
                #body
            }
        };
    }

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
                    Ownership::Owned => {
                        let l = &owned[&edge.target];
                        quote! { #l }
                    }
                }
            }
        })
        .collect()
}

/// Statements building all owned per-test fixtures required (transitively) by
/// `consumer`, in dependency order; returns them plus a fixture→local-ident map.
fn emit_owned(
    discovery: &Discovery,
    graph: &Graph,
    consumer: usize,
) -> Result<(Vec<TokenStream>, HashMap<usize, Ident>)> {
    let mut stmts = Vec::new();
    let mut locals = HashMap::new();
    let mut building = HashSet::new();
    build_owned(
        discovery,
        graph,
        consumer,
        &mut stmts,
        &mut locals,
        &mut building,
    )?;
    Ok((stmts, locals))
}

fn build_owned(
    discovery: &Discovery,
    graph: &Graph,
    consumer: usize,
    stmts: &mut Vec<TokenStream>,
    locals: &mut HashMap<usize, Ident>,
    building: &mut HashSet<usize>,
) -> Result<()> {
    for edge in &graph.nodes[consumer].edges {
        if edge.ownership != Ownership::Owned {
            continue;
        }
        let target = edge.target;
        if locals.contains_key(&target) || !building.insert(target) {
            continue;
        }
        build_owned(discovery, graph, target, stmts, locals, building)?;
        let args = fixture_args(discovery, &graph.nodes[target], locals);
        let local = field_ident(discovery, target);
        let handle = quote! { handle };
        let call = call_tokens(discovery, target, &args, &handle);
        stmts.push(quote! { let #local = #call; });
        locals.insert(target, local);
    }
    Ok(())
}

/// The chain of fixture-defining scopes (shallow → deep) that are ancestors of a
/// group's module. These stay active for the group regardless of whether it uses
/// them, so a common ancestor scope isn't torn down by a sibling that happens not
/// to need it (fixtures within are still built lazily by `ensure`).
fn ancestor_scopes(
    discovery: &Discovery,
    store_fixtures: &[usize],
    group_module: &[String],
) -> Vec<String> {
    let mut by_depth: Vec<(usize, String)> = store_fixtures
        .iter()
        .map(|&i| &discovery.items[i].module_path)
        .filter(|sp| sp.len() <= group_module.len() && group_module[..sp.len()] == sp[..])
        .map(|sp| (sp.len(), scope_key(sp)))
        .collect();
    by_depth.sort();
    by_depth.dedup();
    by_depth.into_iter().map(|(_, k)| k).collect()
}

/// All fixtures reachable from a set of tests, following every edge.
fn reachable(graph: &Graph, tests: &[usize]) -> HashSet<usize> {
    let mut seen = HashSet::new();
    let mut stack: Vec<usize> = tests.to_vec();
    while let Some(node) = stack.pop() {
        for edge in &graph.nodes[node].edges {
            if seen.insert(edge.target) {
                stack.push(edge.target);
            }
        }
    }
    seen
}
