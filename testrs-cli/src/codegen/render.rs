//! Rendering primitives: turn resolved IR types, item paths, and dependency
//! edges into the tokens the harness is assembled from.

use std::collections::HashMap;

use anyhow::{Result, bail};
use proc_macro2::{Ident, TokenStream};
use quote::{format_ident, quote};
use rustdoc_ir::Type;

use crate::discover::Discovery;
use crate::graph::{Node, Ownership};

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

/// Collect the crate roots (first path segment) named by a type, so the harness
/// can declare them as dependencies. Recurses through references and tuples.
pub(super) fn collect_crate_refs(ty: &Type, out: &mut std::collections::BTreeSet<String>) {
    match ty {
        Type::Path(p) => {
            if let Some(root) = p.base_type.first() {
                out.insert(root.clone());
            }
        }
        Type::Reference(r) => collect_crate_refs(&r.inner, out),
        Type::Tuple(t) => {
            for elem in &t.elements {
                collect_crate_refs(elem, out);
            }
        }
        _ => {}
    }
}

/// A resolved type rendered as tokens, for interpolation.
pub(super) fn type_tokens(ty: &Type) -> Result<TokenStream> {
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
pub(super) fn scope_key(module_path: &[String]) -> String {
    if module_path.is_empty() {
        "crate".to_string()
    } else {
        module_path.join("::")
    }
}

/// The store field identifier for a shared fixture.
pub(super) fn field_ident(discovery: &Discovery, idx: usize) -> Ident {
    format_ident!("{}", discovery.items[idx].name)
}

/// `<block_on>(<path>(<args>))` for async items, else the bare call. `block_on`
/// is the discovered `#[runtime]` provider (or `testrs::block_on` by default).
pub(super) fn call_tokens(
    discovery: &Discovery,
    idx: usize,
    args: &[TokenStream],
    block_on: &TokenStream,
) -> TokenStream {
    let path = path_tokens(discovery, idx);
    let call = quote! { #path(#(#args),*) };
    if discovery.items[idx].sig.is_async {
        quote! { #block_on(#call) }
    } else {
        call
    }
}

/// Argument expressions for a node's fixture dependencies: borrowed deps read
/// from the store (`c.<field>`), owned deps from previously-built locals.
pub(super) fn fixture_args(
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
            Ownership::BorrowedMut => {
                let f = field_ident(discovery, edge.target);
                quote! { c.#f.as_mut().unwrap() }
            }
            Ownership::Owned => {
                let l = &owned[&edge.target];
                quote! { #l }
            }
        })
        .collect()
}
