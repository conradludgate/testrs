//! Build and validate the fixture dependency graph.
//!
//! Fixtures are keyed by the `Type` they produce. For each consumer (fixture or
//! test) parameter, we strip a leading `&` to get the underlying type, then find
//! the fixture producing it whose module is the *closest ancestor-or-equal* of
//! the consumer's module — the design's module-tree resolution rule. We classify
//! ownership from the parameter (`&T` borrowed / `T` owned), then validate:
//! missing fixtures, same-level ambiguity, owning a shared ancestor, and cycles.

use std::collections::{HashMap, HashSet};

use rustdoc_ir::Type;
use termtree::Tree;

use crate::discover::{Discovery, MarkerKind, scope_label};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ownership {
    Borrowed,
    Owned,
}

/// A resolved dependency: a consumer parameter wired to a producing fixture.
pub struct Edge {
    pub param: String,
    pub ownership: Ownership,
    /// Index into [`Discovery::items`] of the producing fixture.
    pub target: usize,
}

/// A consumer (fixture or test) and its resolved dependencies.
pub struct Node {
    /// Index into [`Discovery::items`].
    pub item: usize,
    pub edges: Vec<Edge>,
}

pub enum GraphError {
    Missing {
        consumer: String,
        param: String,
        ty: Type,
    },
    Ambiguous {
        consumer: String,
        param: String,
        ty: Type,
        candidates: Vec<String>,
    },
    OwnsAncestor {
        consumer: String,
        param: String,
        fixture: String,
    },
    Cycle {
        path: Vec<String>,
    },
}

pub struct Graph {
    pub nodes: Vec<Node>,
    /// Setup order for fixtures (indices into [`Discovery::items`]).
    pub fixture_order: Vec<usize>,
    pub errors: Vec<GraphError>,
}

/// Strip a single leading reference, returning the underlying type and whether
/// the parameter borrowed it.
fn deref(ty: &Type) -> (&Type, Ownership) {
    match ty {
        Type::Reference(r) => (r.inner.as_ref(), Ownership::Borrowed),
        other => (other, Ownership::Owned),
    }
}

/// True if `ancestor`'s module path is a prefix of (or equal to) `of`'s.
fn is_ancestor_or_equal(ancestor: &[String], of: &[String]) -> bool {
    ancestor.len() <= of.len() && of[..ancestor.len()] == *ancestor
}

pub fn build(discovery: &Discovery) -> Graph {
    let items = &discovery.items;
    let label = |i: usize| format!("{}@{}", items[i].name, scope_label(&items[i].module_path));

    // Index fixtures by the type they produce.
    let mut by_type: HashMap<Type, Vec<usize>> = HashMap::new();
    for (i, it) in items.iter().enumerate() {
        if it.kind == MarkerKind::Fixture
            && let Some(out) = &it.sig.output
        {
            by_type.entry(out.clone()).or_default().push(i);
        }
    }

    let mut nodes = Vec::new();
    let mut errors = Vec::new();

    for (ci, consumer) in items.iter().enumerate() {
        let mut edges = Vec::new();
        for (param, param_ty) in &consumer.sig.inputs {
            // Case parameters are bound to providers, not the fixture graph.
            if consumer.cases.iter().any(|c| &c.param == param) {
                continue;
            }
            let (underlying, ownership) = deref(param_ty);

            let in_scope: Vec<usize> = by_type
                .get(underlying)
                .map_or(&[][..], Vec::as_slice)
                .iter()
                .copied()
                .filter(|&fi| is_ancestor_or_equal(&items[fi].module_path, &consumer.module_path))
                .collect();

            if in_scope.is_empty() {
                errors.push(GraphError::Missing {
                    consumer: label(ci),
                    param: param.clone(),
                    ty: underlying.clone(),
                });
                continue;
            }

            let max_depth = in_scope
                .iter()
                .map(|&fi| items[fi].module_path.len())
                .max()
                .unwrap();
            let closest: Vec<usize> = in_scope
                .into_iter()
                .filter(|&fi| items[fi].module_path.len() == max_depth)
                .collect();

            if closest.len() > 1 {
                errors.push(GraphError::Ambiguous {
                    consumer: label(ci),
                    param: param.clone(),
                    ty: underlying.clone(),
                    candidates: closest.iter().map(|&fi| label(fi)).collect(),
                });
                continue;
            }

            let target = closest[0];
            // Owning (`T`) a fixture defined in a strict ancestor module means
            // moving an instance that's shared at a broader scope — not allowed.
            if ownership == Ownership::Owned && items[target].module_path != consumer.module_path {
                errors.push(GraphError::OwnsAncestor {
                    consumer: label(ci),
                    param: param.clone(),
                    fixture: label(target),
                });
            }

            edges.push(Edge {
                param: param.clone(),
                ownership,
                target,
            });
        }
        nodes.push(Node { item: ci, edges });
    }

    let (fixture_order, cycle) = topo_sort_fixtures(discovery, &nodes);
    if let Some(path) = cycle {
        errors.push(GraphError::Cycle {
            path: path.into_iter().map(label).collect(),
        });
    }

    Graph {
        nodes,
        fixture_order,
        errors,
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Mark {
    Unvisited,
    InProgress,
    Done,
}

/// Topologically sort the fixture subgraph (fixture → fixture edges).
///
/// Returns the setup order and, if the graph has a cycle, one cycle as a list of
/// item indices.
fn topo_sort_fixtures(discovery: &Discovery, nodes: &[Node]) -> (Vec<usize>, Option<Vec<usize>>) {
    // Dependencies among fixtures only.
    let mut deps: HashMap<usize, Vec<usize>> = HashMap::new();
    for node in nodes {
        if discovery.items[node.item].kind != MarkerKind::Fixture {
            continue;
        }
        let fixture_deps = node
            .edges
            .iter()
            .map(|e| e.target)
            .filter(|&t| discovery.items[t].kind == MarkerKind::Fixture)
            .collect();
        deps.insert(node.item, fixture_deps);
    }

    let mut state: HashMap<usize, Mark> = deps.keys().map(|&k| (k, Mark::Unvisited)).collect();
    let mut order = Vec::new();

    // Iterative DFS so a deep graph can't blow the stack; tracks the active path
    // to reconstruct a cycle.
    let mut fixtures: Vec<usize> = deps.keys().copied().collect();
    fixtures.sort_unstable();

    for &start in &fixtures {
        if state[&start] != Mark::Unvisited {
            continue;
        }
        // (node, index of next dependency to visit)
        let mut stack: Vec<(usize, usize)> = vec![(start, 0)];
        state.insert(start, Mark::InProgress);
        while let Some(&(node, next)) = stack.last() {
            let node_deps = &deps[&node];
            if next < node_deps.len() {
                stack.last_mut().unwrap().1 += 1;
                let dep = node_deps[next];
                match state.get(&dep).copied().unwrap_or(Mark::Done) {
                    Mark::Unvisited => {
                        state.insert(dep, Mark::InProgress);
                        stack.push((dep, 0));
                    }
                    Mark::InProgress => {
                        // Found a back-edge: reconstruct the cycle from the path.
                        let start_idx = stack.iter().position(|&(n, _)| n == dep).unwrap();
                        let mut cycle: Vec<usize> =
                            stack[start_idx..].iter().map(|&(n, _)| n).collect();
                        cycle.push(dep);
                        return (order, Some(cycle));
                    }
                    Mark::Done => {}
                }
            } else {
                state.insert(node, Mark::Done);
                order.push(node);
                stack.pop();
            }
        }
    }

    (order, None)
}

pub fn print_graph(discovery: &Discovery, graph: &Graph, invert: bool) {
    let suffix = if invert {
        " (inverted: fixture → dependents)"
    } else {
        ""
    };
    println!("dependency graph for {}{suffix}", discovery.crate_name);

    if invert {
        print_inverse_trees(discovery, graph);
    } else {
        // Each test is the root of its transitive fixture-dependency tree.
        for node in &graph.nodes {
            if discovery.items[node.item].kind != MarkerKind::Test {
                continue;
            }
            let mut tree = Tree::new(format!("[test] {}", node_label(discovery, node.item)));
            let mut path = HashSet::from([node.item]);
            for edge in &node.edges {
                tree.push(edge_tree(discovery, graph, edge, &mut path));
            }
            print!("\n{tree}");
        }
    }

    if !graph.fixture_order.is_empty() {
        let order: Vec<String> = graph
            .fixture_order
            .iter()
            .map(|&i| node_label(discovery, i))
            .collect();
        println!("\nfixture setup order: {}", order.join(", "));
    }

    if graph.errors.is_empty() {
        println!("\nno errors");
    } else {
        println!("\n{} error(s):", graph.errors.len());
        for err in &graph.errors {
            print_error(err);
        }
    }
}

fn node_label(discovery: &Discovery, i: usize) -> String {
    format!(
        "{}@{}",
        discovery.items[i].name,
        scope_label(&discovery.items[i].module_path)
    )
}

/// A subtree for one dependency edge: the edge is the node, the target fixture's
/// own dependencies are its children. A cycle is shown as a leaf, not recursed.
fn edge_tree(
    discovery: &Discovery,
    graph: &Graph,
    edge: &Edge,
    path: &mut HashSet<usize>,
) -> Tree<String> {
    let prefix = match edge.ownership {
        Ownership::Borrowed => "&",
        Ownership::Owned => "",
    };
    let label = format!(
        "{prefix}{} → {}",
        edge.param,
        node_label(discovery, edge.target)
    );
    if !path.insert(edge.target) {
        return Tree::new(format!("{label} (cycle)"));
    }
    let mut tree = Tree::new(label);
    for child in &graph.nodes[edge.target].edges {
        tree.push(edge_tree(discovery, graph, child, path));
    }
    path.remove(&edge.target);
    tree
}

/// Inverse view: each base fixture (one with no dependencies of its own) is a
/// root, and its descendants are the things that depend on it, up to the tests.
fn print_inverse_trees(discovery: &Discovery, graph: &Graph) {
    // For each fixture, the consumers that depend on it (and how).
    let mut dependents: HashMap<usize, Vec<(usize, &Edge)>> = HashMap::new();
    for node in &graph.nodes {
        for edge in &node.edges {
            dependents
                .entry(edge.target)
                .or_default()
                .push((node.item, edge));
        }
    }

    for node in &graph.nodes {
        if discovery.items[node.item].kind != MarkerKind::Fixture || !node.edges.is_empty() {
            continue;
        }
        let mut tree = Tree::new(format!("[fixture] {}", node_label(discovery, node.item)));
        let mut path = HashSet::from([node.item]);
        push_dependents(&mut tree, discovery, &dependents, node.item, &mut path);
        print!("\n{tree}");
    }
}

/// Attach, for each consumer that depends on `idx`, a child subtree (recursing
/// into that consumer's own dependents). A cycle is shown as a leaf.
fn push_dependents(
    tree: &mut Tree<String>,
    discovery: &Discovery,
    dependents: &HashMap<usize, Vec<(usize, &Edge)>>,
    idx: usize,
    path: &mut HashSet<usize>,
) {
    let Some(consumers) = dependents.get(&idx) else {
        return;
    };
    for &(consumer, edge) in consumers {
        let prefix = match edge.ownership {
            Ownership::Borrowed => "&",
            Ownership::Owned => "",
        };
        let label = format!(
            "{prefix}{} ← {}",
            edge.param,
            node_label(discovery, consumer)
        );
        if !path.insert(consumer) {
            tree.push(Tree::new(format!("{label} (cycle)")));
            continue;
        }
        let mut child = Tree::new(label);
        push_dependents(&mut child, discovery, dependents, consumer, path);
        path.remove(&consumer);
        tree.push(child);
    }
}

fn print_error(err: &GraphError) {
    match err {
        GraphError::Missing {
            consumer,
            param,
            ty,
        } => {
            println!("  error: no fixture in scope produces `{ty:?}`");
            println!("    needed by `{param}` in {consumer}");
        }
        GraphError::Ambiguous {
            consumer,
            param,
            ty,
            candidates,
        } => {
            println!("  error: multiple fixtures produce `{ty:?}` at the same scope");
            println!("    needed by `{param}` in {consumer}");
            println!("    candidates: {}", candidates.join(", "));
        }
        GraphError::OwnsAncestor {
            consumer,
            param,
            fixture,
        } => {
            println!("  error: `{param}` in {consumer} takes ownership of `{fixture}`,");
            println!(
                "    but that fixture is shared at a broader scope; borrow it with `&` instead"
            );
        }
        GraphError::Cycle { path } => {
            println!("  error: fixture dependency cycle: {}", path.join(" -> "));
        }
    }
}
