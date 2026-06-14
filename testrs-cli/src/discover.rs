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
use rustdoc_ir::Type;
use rustdoc_processor::CrateCollection;
use rustdoc_processor::cache::RustdocGlobalFsCache;
use rustdoc_processor::compute::NoProgress;
use rustdoc_processor::crate_data::CrateItemIndex;
use rustdoc_processor::indexing::NoAnnotations;
use rustdoc_resolver::{GenericBindings, TypeAliasResolution, resolve_free_function, resolve_type};
use rustdoc_types::{Attribute, Id, ItemEnum};
use syn::parse::Parser;

/// Nightly toolchain used to emit rustdoc JSON for the target crate. Must emit a
/// format version matching the `rustdoc-types` version rustdoc-reflection uses
/// (currently `format_version` 57 == rustdoc-types 0.57.3).
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

/// How a case's value is rendered into its test name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseNameStrategy {
    /// `testrs::TestCaseName::case_name(&value)`.
    TestCaseName,
    /// `{value:?}`.
    Debug,
    /// `{value}`.
    Display,
    /// `<param><index>`.
    Index,
}

/// A test parameter driven by a `#[test(cases(<param> = <provider>))]` binding.
/// The test runs once per element of the cartesian product of all its cases.
pub struct CaseParam {
    /// The test parameter this provider feeds (expects `&Element`).
    pub param: String,
    /// Fully-qualified call path of the provider, e.g. `crate::vectors::vectors`.
    pub provider_call: String,
    /// Element type produced by the provider (the `T` in `Vec<T>`).
    pub element: Type,
    /// How to name each case of this parameter.
    pub name_strategy: CaseNameStrategy,
}

/// Whether a test is expected to panic (`#[test(should_panic)]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShouldPanic {
    No,
    Any,
    With(String),
}

/// A discovered fixture or test, with its module-tree position and signature.
pub struct Discovered {
    pub kind: MarkerKind,
    pub name: String,
    /// Module path relative to the crate root. Empty means the crate root
    /// itself (suite scope).
    pub module_path: Vec<String>,
    pub sig: Signature,
    /// Case bindings (empty for ordinary tests/fixtures).
    pub cases: Vec<CaseParam>,
    /// Panic expectation for a test.
    pub should_panic: ShouldPanic,
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
    /// Directory of the `testrs` crate, if the target depends on it — used to
    /// give the harness access to `testrs::TestCaseName`.
    pub testrs_manifest_dir: Option<PathBuf>,
    pub items: Vec<Discovered>,
}

/// The testrs marker on an item: its kind, plus any `cases(param = provider)`
/// bindings (only meaningful on tests).
///
/// Markers ride in the `diagnostic::testrs::<kind>` namespace, which rustdoc
/// preserves verbatim in `Attribute::Other`.
/// A parsed marker: kind, `cases` bindings (param → provider path), panic expectation.
type ParsedMarker = (MarkerKind, Vec<(String, syn::Path)>, ShouldPanic);

fn parse_marker(attrs: &[Attribute]) -> Option<ParsedMarker> {
    use syn::punctuated::Punctuated;
    use syn::{Expr, ExprLit, Lit, Meta, Token};

    for attr in attrs {
        let Attribute::Other(raw) = attr else {
            continue;
        };
        let Ok(parsed) = syn::Attribute::parse_outer.parse_str(raw) else {
            continue;
        };
        for a in parsed {
            let segs = &a.path().segments;
            if !(segs.len() == 3 && segs[0].ident == "diagnostic" && segs[1].ident == "testrs") {
                continue;
            }
            let kind = match segs[2].ident.to_string().as_str() {
                "fixture" => MarkerKind::Fixture,
                "test" => MarkerKind::Test,
                _ => return None,
            };

            // Parse the marker args: `cases(param = provider, ...)` and
            // `should_panic` / `should_panic = "expected"`.
            let mut cases = Vec::new();
            let mut should_panic = ShouldPanic::No;
            if let Meta::List(list) = &a.meta
                && let Ok(args) =
                    list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
            {
                for arg in args {
                    match arg {
                        Meta::List(inner) if inner.path.is_ident("cases") => {
                            if let Ok(pairs) = inner.parse_args_with(
                                Punctuated::<syn::MetaNameValue, Token![,]>::parse_terminated,
                            ) {
                                for nv in pairs {
                                    if let (Some(param), Expr::Path(provider)) =
                                        (nv.path.get_ident(), &nv.value)
                                    {
                                        cases.push((param.to_string(), provider.path.clone()));
                                    }
                                }
                            }
                        }
                        Meta::Path(p) if p.is_ident("should_panic") => {
                            should_panic = ShouldPanic::Any;
                        }
                        Meta::NameValue(nv) if nv.path.is_ident("should_panic") => {
                            if let Expr::Lit(ExprLit {
                                lit: Lit::Str(s), ..
                            }) = &nv.value
                            {
                                should_panic = ShouldPanic::With(s.value());
                            }
                        }
                        _ => {}
                    }
                }
            }
            return Some((kind, cases, should_panic));
        }
    }
    None
}

/// Resolve a provider path (relative to the test's module, or `crate::`-absolute)
/// to its `(module_path, name)`.
fn provider_location(path: &syn::Path, test_module: &[String]) -> (Vec<String>, String) {
    let segments: Vec<String> = path.segments.iter().map(|s| s.ident.to_string()).collect();
    let (name, prefix) = segments.split_last().expect("non-empty path");
    if prefix.first().is_some_and(|s| s == "crate") {
        (prefix[1..].to_vec(), name.clone())
    } else {
        let mut module = test_module.to_vec();
        module.extend_from_slice(prefix);
        (module, name.clone())
    }
}

/// If the raw rustdoc type is `Vec<T>`, return the raw `T`. We peel before
/// resolving so we never have to resolve `Vec` itself (which needs `alloc`'s docs).
fn raw_vec_element(output: &rustdoc_types::Type) -> Option<&rustdoc_types::Type> {
    let rustdoc_types::Type::ResolvedPath(p) = output else {
        return None;
    };
    if p.path != "Vec" && !p.path.ends_with("::Vec") {
        return None;
    }
    let rustdoc_types::GenericArgs::AngleBracketed { args, .. } = p.args.as_deref()? else {
        return None;
    };
    args.iter().find_map(|arg| match arg {
        rustdoc_types::GenericArg::Type(t) => Some(t),
        _ => None,
    })
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
    let testrs_manifest_dir = graph.packages().find(|p| p.name() == "testrs").map(|p| {
        p.manifest_path()
            .parent()
            .expect("manifest path has a parent")
            .as_std_path()
            .to_path_buf()
    });

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

    let crate_name = krate.crate_name();
    let mut items = Vec::new();
    for (id, item) in index {
        if !matches!(item.inner, ItemEnum::Function(_)) {
            continue;
        }
        let Some((kind, raw_cases, should_panic)) = parse_marker(&item.attrs) else {
            continue;
        };
        let name = item.name.clone().unwrap_or_default();
        let test_module = module_path.get(id).cloned().unwrap_or_default();
        let func = resolve_free_function(
            item,
            krate,
            &collection,
            TypeAliasResolution::ResolveThrough,
        )
        .map_err(|e| anyhow!("resolving `{name}`: {e:?}"))?;

        let mut cases = Vec::new();
        for (param, provider_path) in raw_cases {
            let (pmod, pname) = provider_location(&provider_path, &test_module);
            let provider_item = index
                .values()
                .find(|it| {
                    matches!(it.inner, ItemEnum::Function(_))
                        && it.name.as_deref() == Some(pname.as_str())
                        && module_path.get(&it.id).map(Vec::as_slice) == Some(pmod.as_slice())
                })
                .with_context(|| format!("case provider `{pname}` for `{name}` not found"))?;
            let ItemEnum::Function(provider_fn) = &provider_item.inner else {
                bail!("case provider `{pname}` is not a function");
            };
            if provider_fn.header.is_async {
                bail!("case provider `{pname}` must be synchronous (async providers unsupported)");
            }
            // Peel `Vec<T>` from the raw signature, then resolve just the element `T`.
            let raw_element = provider_fn
                .sig
                .output
                .as_ref()
                .and_then(raw_vec_element)
                .with_context(|| format!("case provider `{pname}` must return `Vec<T>`"))?;
            let element = resolve_type(
                raw_element,
                &pkg_id,
                &collection,
                &GenericBindings::default(),
                TypeAliasResolution::ResolveThrough,
            )
            .map_err(|e| anyhow!("resolving case element type of `{pname}`: {e:?}"))?;

            // Pick the naming strategy from the traits the element implements.
            // Primitives always have Debug; for a local struct/enum we scan its
            // rustdoc impls. Hierarchy: TestCaseName > Debug > Display > index.
            let name_strategy = match raw_element {
                rustdoc_types::Type::Primitive(_) => CaseNameStrategy::Debug,
                rustdoc_types::Type::ResolvedPath(p) => {
                    let impls: &[Id] = index.get(&p.id).map_or(&[][..], |it| match &it.inner {
                        ItemEnum::Struct(s) => s.impls.as_slice(),
                        ItemEnum::Enum(e) => e.impls.as_slice(),
                        _ => &[][..],
                    });
                    let (mut testcasename, mut debug, mut display) = (false, false, false);
                    for impl_id in impls {
                        let Some(ItemEnum::Impl(im)) = index.get(impl_id).map(|it| &it.inner)
                        else {
                            continue;
                        };
                        match im.trait_.as_ref().and_then(|t| t.path.rsplit("::").next()) {
                            Some("TestCaseName") => testcasename = true,
                            Some("Debug") => debug = true,
                            Some("Display") => display = true,
                            _ => {}
                        }
                    }
                    if testcasename {
                        CaseNameStrategy::TestCaseName
                    } else if debug {
                        CaseNameStrategy::Debug
                    } else if display {
                        CaseNameStrategy::Display
                    } else {
                        CaseNameStrategy::Index
                    }
                }
                _ => CaseNameStrategy::Index,
            };

            let mut segments = vec![crate_name.clone()];
            segments.extend(pmod);
            segments.push(pname);
            cases.push(CaseParam {
                param,
                provider_call: segments.join("::"),
                element,
                name_strategy,
            });
        }

        items.push(Discovered {
            kind,
            name,
            module_path: test_module,
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
            cases,
            should_panic,
        });
    }
    items.sort_by(|a, b| (a.kind, &a.module_path, &a.name).cmp(&(b.kind, &b.module_path, &b.name)));

    Ok(Discovery {
        crate_name,
        package_name,
        manifest_dir,
        target_dir,
        testrs_manifest_dir,
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
