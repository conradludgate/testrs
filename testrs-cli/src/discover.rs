//! Discover testrs markers in a target crate and resolve their signatures.
//!
//! This runs `cargo rustdoc` (under a pinned nightly) against the target crate
//! via rustdoc-reflection, scans every item's `attrs` for a
//! `#[diagnostic::testrs::*]` marker, and resolves each marked function's
//! signature into `rustdoc_ir` types — the input to the fixture dependency
//! graph.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use guppy::MetadataCommand;
use rustdoc_processor::CrateCollection;
use rustdoc_processor::cache::RustdocGlobalFsCache;
use rustdoc_processor::compute::NoProgress;
use rustdoc_processor::crate_data::CrateItemIndex;
use rustdoc_processor::indexing::NoAnnotations;
use rustdoc_resolver::{TypeAliasResolution, resolve_free_function};
use rustdoc_types::{Attribute, Item, ItemEnum};
use syn::parse::Parser;

/// Nightly toolchain used to emit rustdoc JSON for the target crate. Must emit
/// a format version matching the `rustdoc-types` version rustdoc-reflection uses
/// (currently format_version 57 == rustdoc-types 0.57.3).
pub const DEFAULT_TOOLCHAIN: &str = "nightly-2026-04-16";

/// The kind of testrs marker found on an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MarkerKind {
    Fixture,
    Test,
}

impl MarkerKind {
    fn label(self) -> &'static str {
        match self {
            MarkerKind::Fixture => "fixture",
            MarkerKind::Test => "test",
        }
    }
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
            let segments: Vec<String> = a
                .path()
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect();
            if let ["diagnostic", "testrs", kind] =
                segments.iter().map(String::as_str).collect::<Vec<_>>()[..]
            {
                match kind {
                    "fixture" => return Some(MarkerKind::Fixture),
                    "test" => return Some(MarkerKind::Test),
                    _ => {}
                }
            }
        }
    }
    None
}

pub fn run(manifest_path: &Path, package: &str, toolchain: &str) -> Result<()> {
    let graph = MetadataCommand::new()
        .manifest_path(manifest_path)
        .build_graph()
        .with_context(|| format!("running `cargo metadata` for {}", manifest_path.display()))?;

    let pkg = graph
        .packages()
        .find(|p| p.name() == package)
        .with_context(|| format!("package `{package}` not found in the workspace"))?;
    let pkg_id = pkg.id().clone();

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

    let mut marked: Vec<(MarkerKind, String, &Item)> = Vec::new();
    for item in index.values() {
        if !matches!(item.inner, ItemEnum::Function(_)) {
            continue;
        }
        if let Some(kind) = marker_kind(&item.attrs) {
            marked.push((kind, item.name.clone().unwrap_or_default(), item));
        }
    }
    marked.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));

    println!("{package}: discovered {} testrs item(s)", marked.len());
    for (kind, name, item) in &marked {
        let func = resolve_free_function(item, krate, &collection, TypeAliasResolution::ResolveThrough)
            .map_err(|e| anyhow!("resolving `{name}`: {e:?}"))?;

        let asyncness = if func.header.is_async { " async" } else { "" };
        println!("\n[{}]{asyncness} {name}", kind.label());
        for input in &func.header.inputs {
            println!("    {}: {:?}", input.name, input.type_);
        }
        match &func.header.output {
            Some(ty) => println!("    -> {ty:?}"),
            None => println!("    -> ()"),
        }
    }

    Ok(())
}
