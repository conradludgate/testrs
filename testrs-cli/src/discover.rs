//! Discover testrs markers in a target crate and resolve their signatures.
//!
//! This runs `cargo rustdoc` (under a pinned nightly) against the target crate
//! via rustdoc-reflection, scans every item's `attrs` for a
//! `#[diagnostic::testrs::*]` marker, records each marked function's position in
//! the module tree, and resolves its signature into `rustdoc_ir` types — the
//! input to the fixture dependency graph.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use guppy::MetadataCommand;
use rustdoc_processor::CrateCollection;
use rustdoc_processor::cache::RustdocGlobalFsCache;
use rustdoc_processor::compute::NoProgress;
use rustdoc_processor::crate_data::CrateItemIndex;
use rustdoc_processor::indexing::NoAnnotations;
use rustdoc_resolver::{TypeAliasResolution, resolve_free_function};
use rustdoc_types::{Attribute, Id, ItemEnum};
use syn::parse::Parser;

/// Nightly toolchain used to emit rustdoc JSON for the target crate. Must emit a
/// format version matching the `rustdoc-types` version rustdoc-reflection uses
/// (currently format_version 57 == rustdoc-types 0.57.3).
pub const DEFAULT_TOOLCHAIN: &str = "nightly-2026-04-16";

/// The kind of testrs marker found on an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MarkerKind {
    Fixture,
    Test,
}

impl MarkerKind {
    pub fn label(self) -> &'static str {
        match self {
            MarkerKind::Fixture => "fixture",
            MarkerKind::Test => "test",
        }
    }
}

/// A resolved function signature, decoupled from rustdoc-reflection's types.
pub struct Signature {
    pub inputs: Vec<(String, rustdoc_ir::Type)>,
    pub output: Option<rustdoc_ir::Type>,
    pub is_async: bool,
}

/// A discovered fixture or test, with its module-tree position and signature.
pub struct Discovered {
    pub kind: MarkerKind,
    pub name: String,
    /// Module path relative to the crate root. Empty means the crate root
    /// itself (suite scope).
    pub module_path: Vec<String>,
    pub sig: Signature,
}

/// All testrs items discovered in a crate.
pub struct Discovery {
    /// The crate (lib) name, underscored — used in generated code paths.
    pub crate_name: String,
    /// The cargo package name — used as the harness crate's path-dependency key.
    pub package_name: String,
    /// Directory containing the analyzed package's `Cargo.toml`.
    pub manifest_dir: PathBuf,
    /// The workspace target directory (where the ephemeral harness crate lives).
    pub target_dir: PathBuf,
    pub items: Vec<Discovered>,
}

/// Return the testrs marker kind carried by an item's attributes, if any.
///
/// Markers ride in the `diagnostic::testrs::<kind>` namespace, which rustdoc
/// preserves verbatim in `Attribute::Other`.
fn marker_kind(attrs: &[Attribute]) -> Option<MarkerKind> {
    for attr in attrs {
        let Attribute::Other(raw) = attr else { continue };
        let Ok(parsed) = syn::Attribute::parse_outer.parse_str(raw) else {
            continue;
        };
        for a in parsed {
            let segs = &a.path().segments;
            if segs.len() == 3 && segs[0].ident == "diagnostic" && segs[1].ident == "testrs" {
                return match segs[2].ident.to_string().as_str() {
                    "fixture" => Some(MarkerKind::Fixture),
                    "test" => Some(MarkerKind::Test),
                    _ => None,
                };
            }
        }
    }
    None
}

pub fn discover(manifest_path: &Path, package: &str, toolchain: &str) -> Result<Discovery> {
    let graph = MetadataCommand::new()
        .manifest_path(manifest_path)
        .build_graph()
        .with_context(|| format!("running `cargo metadata` for {}", manifest_path.display()))?;

    let pkg = graph
        .packages()
        .find(|p| p.name() == package)
        .with_context(|| format!("package `{package}` not found in the workspace"))?;
    let pkg_id = pkg.id().clone();
    let package_name = pkg.name().to_string();
    let manifest_dir = pkg
        .manifest_path()
        .parent()
        .expect("manifest path has a parent")
        .as_std_path()
        .to_path_buf();
    let target_dir = graph
        .workspace()
        .target_directory()
        .as_std_path()
        .to_path_buf();

    let cache_dir = std::env::temp_dir().join("testrs-rustdoc-cache");
    std::fs::create_dir_all(&cache_dir)?;
    let cache = RustdocGlobalFsCache::new("testrs-0.1", toolchain, false, &graph, &cache_dir)
        .context("opening the rustdoc cache")?;

    let collection = CrateCollection::new(
        NoAnnotations,
        toolchain.to_string(),
        graph.clone(),
        "testrs-0.1".to_string(),
        cache,
        Box::new(NoProgress),
    );

    let krate = collection
        .get_or_compute(&pkg_id)
        .map_err(|e| anyhow!("generating rustdoc JSON for `{package}`: {e:?}"))?;

    let index = match &krate.core.krate.index {
        CrateItemIndex::Eager(e) => &e.index,
        CrateItemIndex::Lazy(_) => {
            bail!("expected an eager item index for a freshly computed crate")
        }
    };

    // Walk the module tree from the crate root, recording each item's containing
    // module path. This covers private items (which `--document-private-items`
    // keeps), and is the tree the fixture scoping rules operate on.
    let mut module_path: HashMap<Id, Vec<String>> = HashMap::new();
    let mut stack: Vec<(Id, Vec<String>)> = vec![(krate.core.krate.root_item_id, Vec::new())];
    while let Some((id, prefix)) = stack.pop() {
        let Some(item) = index.get(&id) else { continue };
        let ItemEnum::Module(module) = &item.inner else {
            continue;
        };
        for &child in &module.items {
            let Some(child_item) = index.get(&child) else {
                continue;
            };
            if matches!(child_item.inner, ItemEnum::Module(_)) {
                let mut nested = prefix.clone();
                nested.push(child_item.name.clone().unwrap_or_default());
                stack.push((child, nested));
            } else {
                module_path.insert(child, prefix.clone());
            }
        }
    }

    let mut items = Vec::new();
    for (id, item) in index {
        if !matches!(item.inner, ItemEnum::Function(_)) {
            continue;
        }
        let Some(kind) = marker_kind(&item.attrs) else {
            continue;
        };
        let name = item.name.clone().unwrap_or_default();
        let func = resolve_free_function(item, krate, &collection, TypeAliasResolution::ResolveThrough)
            .map_err(|e| anyhow!("resolving `{name}`: {e:?}"))?;
        items.push(Discovered {
            kind,
            name,
            module_path: module_path.get(id).cloned().unwrap_or_default(),
            sig: Signature {
                inputs: func
                    .header
                    .inputs
                    .iter()
                    .map(|i| (i.name.to_string(), i.type_.clone()))
                    .collect(),
                output: func.header.output.clone(),
                is_async: func.header.is_async,
            },
        });
    }
    items.sort_by(|a, b| (a.kind, &a.module_path, &a.name).cmp(&(b.kind, &b.module_path, &b.name)));

    Ok(Discovery {
        crate_name: krate.crate_name(),
        package_name,
        manifest_dir,
        target_dir,
        items,
    })
}

/// Render a module path relative to the crate root for display.
pub fn scope_label(module_path: &[String]) -> String {
    if module_path.is_empty() {
        "crate".to_string()
    } else {
        format!("crate::{}", module_path.join("::"))
    }
}

pub fn print_discovery(discovery: &Discovery) {
    println!(
        "{}: discovered {} testrs item(s)",
        discovery.crate_name,
        discovery.items.len()
    );
    for item in &discovery.items {
        let asyncness = if item.sig.is_async { " async" } else { "" };
        println!(
            "\n[{}]{asyncness} {}  (module: {})",
            item.kind.label(),
            item.name,
            scope_label(&item.module_path),
        );
        for (name, ty) in &item.sig.inputs {
            println!("    {name}: {ty:?}");
        }
        match &item.sig.output {
            Some(ty) => println!("    -> {ty:?}"),
            None => println!("    -> ()"),
        }
    }
}
