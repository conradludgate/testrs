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
//! The emitter writes the harness as a string: fixed scaffolding is `push_str`'d
//! verbatim (raw strings, so the generated code reads as-is here), while the
//! graph-dependent lines are `writeln!`'d.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write;

use anyhow::{Result, bail};
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

/// `crate::path::to::name` for a discovered item, from the harness target's view.
fn item_path(discovery: &Discovery, idx: usize) -> String {
    let item = &discovery.items[idx];
    let mut segments = vec![discovery.crate_name.clone()];
    segments.extend(item.module_path.iter().cloned());
    segments.push(item.name.clone());
    segments.join("::")
}

/// Scope/group key for a module path: `crate` at the root, else the joined path.
fn scope_key(module_path: &[String]) -> String {
    if module_path.is_empty() {
        "crate".to_string()
    } else {
        module_path.join("::")
    }
}

/// Call expression for a fixture/test, wrapping async ones in `<handle>.block_on`.
fn call_expr(discovery: &Discovery, idx: usize, args: &[String], handle: &str) -> String {
    let path = item_path(discovery, idx);
    let call = format!("{path}({})", args.join(", "));
    if discovery.items[idx].sig.is_async {
        format!("{handle}.block_on({call})")
    } else {
        call
    }
}

/// Argument list for a call inside a `FIXTURES` borrow: borrowed deps come from
/// the store (`c.<field>`), owned deps from previously-built locals.
fn call_args(
    discovery: &Discovery,
    node: &Node,
    owned_locals: &HashMap<usize, String>,
) -> Vec<String> {
    node.edges
        .iter()
        .map(|edge| match edge.ownership {
            Ownership::Borrowed => {
                format!("c.{}.as_ref().unwrap()", discovery.items[edge.target].name)
            }
            Ownership::Owned => owned_locals[&edge.target].clone(),
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
            let shared_needed: Vec<usize> = graph
                .fixture_order
                .iter()
                .copied()
                .filter(|i| shared.contains(i) && reachable.contains(i))
                .collect();
            (module_path.clone(), tests.clone(), shared_needed)
        })
        .collect();

    // Shared fixtures used by any group, in setup (topo) order.
    let store_fixtures: Vec<usize> = graph
        .fixture_order
        .iter()
        .copied()
        .filter(|i| shared.contains(i) && group_shared.iter().any(|(_, _, s)| s.contains(i)))
        .collect();

    let field = |i: usize| discovery.items[i].name.clone();
    let mut out = String::new();

    out.push_str(
        r"// @generated by testrs — do not edit.
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
",
    );
    for &fi in &store_fixtures {
        let ty = render_type(discovery.items[fi].sig.output.as_ref().unwrap())?;
        writeln!(out, "    {}: Option<{ty}>,", field(fi))?;
    }
    out.push_str(
        r"}
thread_local! {
    static FIXTURES: RefCell<Fixtures> = RefCell::new(Fixtures::default());
}

struct FixtureRunner { handle: Handle, active: RefCell<Vec<&'static str>> }

impl FixtureRunner {
",
    );
    // `ensure_<fixture>` builds a shared fixture once, after its borrowed deps.
    for &fi in &store_fixtures {
        let name = field(fi);
        let node = &graph.nodes[fi];
        if node.edges.iter().any(|e| e.ownership == Ownership::Owned) {
            bail!("owned dependency of a shared fixture is not supported yet");
        }
        writeln!(out, "    fn ensure_{name}(&self) {{")?;
        writeln!(
            out,
            "        if FIXTURES.with(|c| c.borrow().{name}.is_some()) {{ return; }}"
        )?;
        for edge in &node.edges {
            writeln!(out, "        self.ensure_{}();", field(edge.target))?;
        }
        if node.edges.is_empty() {
            writeln!(
                out,
                "        let value = {};",
                call_expr(discovery, fi, &[], "self.handle")
            )?;
        } else {
            let args = call_args(discovery, node, &HashMap::new());
            writeln!(out, "        let value = FIXTURES.with(|c| {{")?;
            writeln!(out, "            let c = c.borrow();")?;
            writeln!(
                out,
                "            {}",
                call_expr(discovery, fi, &args, "self.handle")
            )?;
            writeln!(out, "        }});")?;
        }
        writeln!(
            out,
            "        FIXTURES.with(|c| c.borrow_mut().{name} = Some(value));"
        )?;
        writeln!(out, "    }}")?;
    }
    // `ensure(group)` builds every shared fixture the group needs.
    out.push_str("    fn ensure(&self, group: &str) {\n        match group {\n");
    for (module_path, _tests, shared_needed) in &group_shared {
        writeln!(out, "            {:?} => {{", scope_key(module_path))?;
        for &fi in shared_needed {
            writeln!(out, "                self.ensure_{}();", field(fi))?;
        }
        writeln!(out, "            }}")?;
    }
    out.push_str(
        r"            _ => {}
        }
    }
}

fn teardown_scope(scope: &str) {
    FIXTURES.with(|c| {
        let mut c = c.borrow_mut();
        match scope {
",
    );
    // Tear down a scope's fixtures (reverse topo) when the runner leaves it.
    let scopes: BTreeSet<String> = store_fixtures
        .iter()
        .map(|&i| scope_key(&discovery.items[i].module_path))
        .collect();
    for scope in &scopes {
        write!(out, "            {scope:?} => {{")?;
        for &fi in store_fixtures.iter().rev() {
            if scope_key(&discovery.items[fi].module_path) == *scope {
                write!(out, " c.{} = None;", field(fi))?;
            }
        }
        writeln!(out, " }}")?;
    }
    out.push_str(
        r"            _ => {}
        }
    });
}

fn common_prefix(a: &[&str], b: &[&str]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

impl<'t> TestGroupRunner<'t, &'static str, &'static str, ()> for FixtureRunner {
    fn run_group<F>(&self, f: F, key: &&'static str, _ctx: Option<&()>)
        -> ControlFlow<TestGroupOutcomes<'t>, TestGroupOutcomes<'t>>
    where F: FnOnce() -> TestGroupOutcomes<'t> {
        let target: &[&'static str] = match *key {
",
    );
    // Each group's ancestor scope chain (kept active even when unused, so a
    // shared scope isn't torn down by a sibling that doesn't need it).
    for (module_path, _tests, _shared_needed) in &group_shared {
        let chain = ancestor_scopes(discovery, &store_fixtures, module_path);
        let rendered: Vec<String> = chain.iter().map(|s| format!("{s:?}")).collect();
        writeln!(
            out,
            "            {:?} => &[{}],",
            scope_key(module_path),
            rendered.join(", ")
        )?;
    }
    out.push_str(
        r"            _ => &[],
        };
        {
            let mut active = self.active.borrow_mut();
            let common = common_prefix(&active, target);
            for scope in active[common..].iter().rev() { teardown_scope(scope); }
            active.truncate(common);
            active.extend_from_slice(&target[common..]);
        }
        self.ensure(*key);
        ControlFlow::Continue(f())
    }
}

fn tests(handle: Handle) -> Vec<Test<&'static str>> {
    let mut tests = Vec::new();
",
    );
    // One `tests.push(...)` per test (wrapped in product loops for `cases`).
    for (module_path, tests, _shared) in &group_shared {
        let key = scope_key(module_path);
        for &ti in tests {
            emit_test(&mut out, discovery, graph, ti, &key)?;
        }
    }
    out.push_str(
        r#"    tests
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
    let selected: Vec<_> = all.into_iter().filter(|t| args.matches(&t.meta.name)).collect();
    kitest::harness(&selected)
        .with_grouper(|m: &TestMeta<&'static str>| m.extra)
        .with_groups(kitest::group::TestGroupBTreeMap::new())
        .with_group_runner(FixtureRunner { handle, active: RefCell::new(Vec::new()) })
        .with_runner(SimpleRunner::default())
        .run()
        .exit_code()
}
"#,
    );

    Ok(out)
}

/// Emit the `tests.push(...)` for one test — wrapped in cartesian-product loops
/// over its `cases` providers (each leaked to `'static`) when it has any.
fn emit_test(
    out: &mut String,
    discovery: &Discovery,
    graph: &Graph,
    ti: usize,
    key: &str,
) -> Result<()> {
    let item = &discovery.items[ti];
    // Qualify the test name with its module path (nextest's `::` convention) so
    // the group is visible in the output: `users::test_find_user`.
    let name = if item.module_path.is_empty() {
        item.name.clone()
    } else {
        format!("{}::{}", item.module_path.join("::"), item.name)
    };

    // Leak each provider's collection so `&T` cases live for the whole run, then
    // open one loop per provider — their product yields the test instances.
    for case in &item.cases {
        let elem = render_type(&case.element)?;
        writeln!(
            out,
            "    let {p}_cases: &'static [{elem}] = Vec::leak({call}().into_iter().collect::<Vec<_>>());",
            p = case.param,
            call = case.provider_call,
        )?;
    }
    for case in &item.cases {
        writeln!(
            out,
            "    for ({p}_i, {p}) in {p}_cases.iter().enumerate() {{",
            p = case.param
        )?;
    }

    // Cases render as `name{param0=value0,param1=value1}`. `{{`/`}}` are the
    // literal braces; per dimension we emit `<param>=<placeholder>`.
    let name_expr = if item.cases.is_empty() {
        format!("{name:?}.into()")
    } else {
        let mut fmt = name.clone();
        fmt.push_str("{{");
        let mut fmt_args = Vec::new();
        for (i, case) in item.cases.iter().enumerate() {
            if i > 0 {
                fmt.push(',');
            }
            fmt.push_str(&case.param);
            fmt.push('=');
            match case.name_strategy {
                CaseNameStrategy::Index => {
                    fmt.push_str("{}");
                    fmt_args.push(format!("{}_i", case.param));
                }
                CaseNameStrategy::Debug => {
                    fmt.push_str("{:?}");
                    fmt_args.push(case.param.clone());
                }
                CaseNameStrategy::Display => {
                    fmt.push_str("{}");
                    fmt_args.push(case.param.clone());
                }
                CaseNameStrategy::TestCaseName => {
                    fmt.push_str("{}");
                    fmt_args.push(format!("testrs::TestCaseName::case_name({})", case.param));
                }
            }
        }
        fmt.push_str("}}");
        format!("format!({fmt:?}, {}).into()", fmt_args.join(", "))
    };

    let (owned_stmts, owned_locals) = emit_owned(discovery, graph, ti)?;
    let args = test_call_args(discovery, graph, ti, &owned_locals);
    let call = call_expr(discovery, ti, &args, "handle");

    writeln!(out, "        tests.push(Test::new(")?;
    writeln!(out, "            TestFnHandle::from_boxed({{")?;
    writeln!(out, "                let handle = handle.clone();")?;
    writeln!(out, "                move || {{")?;
    writeln!(out, "                    FIXTURES.with(|c| {{")?;
    writeln!(out, "                        let c = c.borrow();")?;
    for stmt in &owned_stmts {
        writeln!(out, "                        {stmt}")?;
    }
    writeln!(out, "                        {call};")?;
    writeln!(out, "                    }});")?;
    writeln!(out, "                }}")?;
    writeln!(out, "            }}),")?;
    let should_panic = match &item.should_panic {
        ShouldPanic::No => String::new(),
        ShouldPanic::Any => "should_panic: PanicExpectation::ShouldPanic, ".to_string(),
        ShouldPanic::With(msg) => {
            format!("should_panic: PanicExpectation::ShouldPanicWithExpected({msg:?}.into()), ")
        }
    };
    writeln!(
        out,
        "            TestMeta {{ name: {name_expr}, extra: {key:?}, {should_panic}..Default::default() }},"
    )?;
    writeln!(out, "        ));")?;

    for _ in &item.cases {
        writeln!(out, "    }}")?;
    }
    Ok(())
}

/// Argument list for invoking a test: case params use their loop variable;
/// fixture params come from the store (borrowed) or an owned local.
fn test_call_args(
    discovery: &Discovery,
    graph: &Graph,
    ti: usize,
    owned_locals: &HashMap<usize, String>,
) -> Vec<String> {
    let item = &discovery.items[ti];
    let node = &graph.nodes[ti];
    item.sig
        .inputs
        .iter()
        .map(|(param, _ty)| {
            if item.cases.iter().any(|c| &c.param == param) {
                param.clone()
            } else {
                let edge = node
                    .edges
                    .iter()
                    .find(|e| &e.param == param)
                    .expect("fixture edge for non-case parameter");
                match edge.ownership {
                    Ownership::Borrowed => {
                        format!("c.{}.as_ref().unwrap()", discovery.items[edge.target].name)
                    }
                    Ownership::Owned => owned_locals[&edge.target].clone(),
                }
            }
        })
        .collect()
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

/// Emit statements building all owned per-test fixtures required (transitively)
/// by `consumer`, in dependency order; returns them plus a fixture→local map.
fn emit_owned(
    discovery: &Discovery,
    graph: &Graph,
    consumer: usize,
) -> Result<(Vec<String>, HashMap<usize, String>)> {
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
    stmts: &mut Vec<String>,
    locals: &mut HashMap<usize, String>,
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
        let args = call_args(discovery, &graph.nodes[target], locals);
        let local = discovery.items[target].name.clone();
        stmts.push(format!(
            "let {local} = {};",
            call_expr(discovery, target, &args, "handle")
        ));
        locals.insert(target, local);
    }
    Ok(())
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
