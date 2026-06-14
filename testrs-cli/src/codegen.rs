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
//! The work is split across submodules: [`render`] turns resolved IR into
//! tokens, [`test_fn`] emits a single test's block, and [`generate`] here
//! assembles the store, scope machinery, and `main`.

mod render;
mod test_fn;

use std::collections::{BTreeSet, HashMap, HashSet};

use anyhow::{Result, anyhow, bail};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};

use crate::discover::{Discovery, MarkerKind};
use crate::graph::{Graph, Ownership};
use render::{call_tokens, field_ident, fixture_args, scope_key, type_tokens};
use test_fn::emit_test;

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
