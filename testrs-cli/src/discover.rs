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

/// Whether a test is expected to panic (`#[panics]` / `#[panics("msg")]`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ShouldPanic {
    #[default]
    No,
    Any,
    With(String),
}

/// A `#[skip(if = ..., reason = ...)]` modifier: a generated predicate the harness
/// evaluates (with the test's fixtures) to decide, at run time, whether to skip.
pub struct Skip {
    /// Fully-qualified call path of the generated `-> bool` predicate, e.g.
    /// `crate_name::module::__testrs_skip_my_test`.
    pub call: String,
    /// The reason reported when the test is skipped (defaults to the condition's
    /// source text in the macro, so this is always populated in practice).
    pub reason: Option<String>,
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
    /// Conditional run-time skips, one per `#[skip(...)]` (the test skips on the
    /// first whose condition holds). Empty for tests without `#[skip]`.
    pub skip: Vec<Skip>,
}

/// A `#[tear_down]` function: cleanup for the fixture it consumes by value, run
/// (sync or async) when that fixture's scope ends.
pub struct TearDown {
    /// Module path of the teardown function, for scope resolution.
    pub module_path: Vec<String>,
    /// The fixture type it tears down — its sole, by-value parameter.
    pub subject: Type,
    /// Fully-qualified call path, e.g. `crate_name::sqlite::session::drop_events`.
    pub call: String,
    /// Whether it's async (driven through the `#[runtime]` bridge).
    pub is_async: bool,
}

/// A direct (external) dependency of the analyzed crate. Used to add it to the
/// ephemeral harness when a shared fixture or case type names it.
pub struct ExternalDep {
    /// The cargo package name (the `[dependencies]` key).
    pub package: String,
    /// The exact resolved version, e.g. `0.32.1`.
    pub version: String,
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
    /// The analyzed crate's external (registry) dependencies, keyed by the name
    /// used in code (`-` → `_`). The harness adds any it references for a shared
    /// fixture or case type — e.g. a fixture returning `rusqlite::Connection`.
    pub dependencies: HashMap<String, ExternalDep>,
    /// Fully-qualified path of the `#[runtime]` async bridge, if the crate marks
    /// one (e.g. `crate_name::module::rt`). `None` falls back to `testrs::block_on`.
    pub runtime_call: Option<String>,
    /// `#[tear_down]` functions, each linked to a fixture by the graph.
    pub tear_downs: Vec<TearDown>,
    pub items: Vec<Discovered>,
}

/// The primary testrs marker on a function — mutually exclusive.
#[derive(Clone, Copy, PartialEq)]
enum Primary {
    Fixture,
    Test,
    Runtime,
    TearDown,
}

/// The testrs markers found on one item. Each `#[test]`/`#[cases]`/`#[panics]`/…
/// is its own `#[diagnostic::testrs::<kind>(...)]` attribute (the namespace
/// rustdoc preserves verbatim in `Attribute::Other`); an item merges several.
#[derive(Default)]
struct Markers {
    primary: Option<Primary>,
    /// `cases(param = provider, ...)` bindings.
    cases: Vec<(String, syn::Path)>,
    /// `panics` / `panics("msg")`.
    should_panic: ShouldPanic,
    /// `skip(cond = predicate, reason = "...")` markers (one per `#[skip]`): each
    /// is the predicate path and its reason.
    skip: Vec<(syn::Path, Option<String>)>,
}

/// Collect every `#[diagnostic::testrs::*]` marker on an item into one [`Markers`].
fn parse_markers(attrs: &[Attribute]) -> Markers {
    use syn::punctuated::Punctuated;
    use syn::{Expr, Meta, Token};

    let mut markers = Markers::default();
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
            match segs[2].ident.to_string().as_str() {
                "fixture" => markers.primary = Some(Primary::Fixture),
                "test" => markers.primary = Some(Primary::Test),
                "runtime" => markers.primary = Some(Primary::Runtime),
                "tear_down" => markers.primary = Some(Primary::TearDown),
                "cases" => {
                    if let Meta::List(list) = &a.meta
                        && let Ok(pairs) = list.parse_args_with(
                            Punctuated::<syn::MetaNameValue, Token![,]>::parse_terminated,
                        )
                    {
                        for nv in pairs {
                            if let (Some(param), Expr::Path(provider)) =
                                (nv.path.get_ident(), &nv.value)
                            {
                                markers
                                    .cases
                                    .push((param.to_string(), provider.path.clone()));
                            }
                        }
                    }
                }
                "panics" => {
                    markers.should_panic = match &a.meta {
                        // `#[panics("substring")]`
                        Meta::List(list) => list
                            .parse_args::<syn::LitStr>()
                            .map_or(ShouldPanic::Any, |s| ShouldPanic::With(s.value())),
                        // `#[panics]`
                        _ => ShouldPanic::Any,
                    };
                }
                "skip" => {
                    if let Meta::List(list) = &a.meta
                        && let Ok(pairs) = list.parse_args_with(
                            Punctuated::<syn::MetaNameValue, Token![,]>::parse_terminated,
                        )
                    {
                        let (mut cond, mut reason) = (None, None);
                        for nv in pairs {
                            match nv.path.get_ident().map(ToString::to_string).as_deref() {
                                Some("cond") => {
                                    if let Expr::Path(p) = &nv.value {
                                        cond = Some(p.path.clone());
                                    }
                                }
                                Some("reason") => {
                                    if let Expr::Lit(syn::ExprLit {
                                        lit: syn::Lit::Str(s),
                                        ..
                                    }) = &nv.value
                                    {
                                        reason = Some(s.value());
                                    }
                                }
                                _ => {}
                            }
                        }
                        if let Some(cond) = cond {
                            markers.skip.push((cond, reason));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    markers
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

    // The analyzed crate's external dependencies, so the harness can name their
    // types (e.g. a shared fixture returning `rusqlite::Connection`). We pin the
    // exact resolved version, so the harness compiles against the same crate the
    // analyzed package does (cargo unifies their features within the one build).
    let dependencies: HashMap<String, ExternalDep> = pkg
        .direct_links()
        .map(|link| link.to())
        .filter(|dep| dep.source().is_external())
        .map(|dep| {
            (
                dep.name().replace('-', "_"),
                ExternalDep {
                    package: dep.name().to_string(),
                    version: dep.version().to_string(),
                },
            )
        })
        .collect();

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
    let mut runtime_call: Option<String> = None;
    let mut tear_downs: Vec<TearDown> = Vec::new();
    for (id, item) in index {
        if !matches!(item.inner, ItemEnum::Function(_)) {
            continue;
        }
        // A function may carry several testrs markers (`test` + `cases` + `panics`,
        // etc.); collect and merge them.
        let markers = parse_markers(&item.attrs);
        if markers.primary == Some(Primary::Runtime) {
            // The async bridge: record its path, don't treat it as a fixture
            // (its signature is generic and never enters the graph).
            let name = item.name.clone().unwrap_or_default();
            let module = module_path.get(id).cloned().unwrap_or_default();
            let mut segments = vec![crate_name.clone()];
            segments.extend(module);
            segments.push(name);
            if runtime_call.replace(segments.join("::")).is_some() {
                bail!("multiple `#[testrs::runtime]` functions found; only one is allowed");
            }
            continue;
        }
        if markers.primary == Some(Primary::TearDown) {
            // A teardown function: not a graph node — it consumes a fixture by
            // value when that fixture's scope ends. Record its subject type so
            // the graph can link it to the fixture it tears down.
            let name = item.name.clone().unwrap_or_default();
            let module = module_path.get(id).cloned().unwrap_or_default();
            let func = resolve_free_function(
                item,
                krate,
                &collection,
                TypeAliasResolution::ResolveThrough,
            )
            .map_err(|e| anyhow!("resolving teardown `{name}`: {e:?}"))?;
            let inputs = &func.header.inputs;
            if inputs.len() != 1 {
                bail!(
                    "`#[tear_down]` `{name}` must take exactly one parameter (the fixture it tears down)"
                );
            }
            if matches!(inputs[0].type_, Type::Reference(_)) {
                bail!("`#[tear_down]` `{name}` must take its fixture by value, not by reference");
            }
            let subject = inputs[0].type_.clone();
            let mut segments = vec![crate_name.clone()];
            segments.extend(module.clone());
            segments.push(name);
            tear_downs.push(TearDown {
                module_path: module,
                subject,
                call: segments.join("::"),
                is_async: func.header.is_async,
            });
            continue;
        }
        let has_modifiers = !markers.cases.is_empty()
            || markers.should_panic != ShouldPanic::No
            || !markers.skip.is_empty();
        let kind = match markers.primary {
            Some(Primary::Test) => MarkerKind::Test,
            Some(Primary::Fixture) => MarkerKind::Fixture,
            // Runtime/TearDown are handled above; None means no `#[test]`.
            _ => {
                if has_modifiers {
                    let name = item.name.clone().unwrap_or_default();
                    bail!("`{name}` has `#[cases]`/`#[panics]`/`#[skip]` but no `#[test]`");
                }
                continue;
            }
        };
        // `#[cases]`/`#[panics]`/`#[skip]` are test-only.
        if kind == MarkerKind::Fixture && has_modifiers {
            let name = item.name.clone().unwrap_or_default();
            bail!(
                "`{name}`: `#[cases]`/`#[panics]`/`#[skip]` are only valid on `#[test]`, not `#[fixture]`"
            );
        }
        let raw_cases = markers.cases;
        let should_panic = markers.should_panic;
        let raw_skip = markers.skip;
        let name = item.name.clone().unwrap_or_default();
        let test_module = module_path.get(id).cloned().unwrap_or_default();
        let func = resolve_free_function(
            item,
            krate,
            &collection,
            TypeAliasResolution::ResolveThrough,
        )
        .map_err(|e| anyhow!("resolving `{name}`: {e:?}"))?;
        let ItemEnum::Function(test_fn) = &item.inner else {
            continue;
        };

        let mut cases = Vec::new();
        for (param, provider_path) in raw_cases {
            let (pmod, pname) = provider_location(&provider_path, &test_module);

            // The element type comes from the test parameter (`param: &T` ⇒ `T`),
            // so any `IntoIterator` source works regardless of the provider's own
            // return type.
            let raw_param = test_fn
                .sig
                .inputs
                .iter()
                .find(|(n, _)| *n == param)
                .map(|(_, t)| t)
                .with_context(|| format!("test `{name}` has no parameter `{param}` for `cases`"))?;
            let raw_element = match raw_param {
                rustdoc_types::Type::BorrowedRef { type_, .. } => type_.as_ref(),
                other => other,
            };
            let element = resolve_type(
                raw_element,
                &pkg_id,
                &collection,
                &GenericBindings::default(),
                TypeAliasResolution::ResolveThrough,
            )
            .map_err(|e| anyhow!("resolving case element type for `{param}` in `{name}`: {e:?}"))?;

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

        // Resolve each `#[skip]` predicate path (relative to the test's module) to
        // its fully-qualified call path, like a `cases` provider.
        let skip = raw_skip
            .into_iter()
            .map(|(cond_path, reason)| {
                let (smod, sname) = provider_location(&cond_path, &test_module);
                let mut segments = vec![crate_name.clone()];
                segments.extend(smod);
                segments.push(sname);
                Skip {
                    call: segments.join("::"),
                    reason,
                }
            })
            .collect();

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
            skip,
        });
    }
    items.sort_by(|a, b| (a.kind, &a.module_path, &a.name).cmp(&(b.kind, &b.module_path, &b.name)));

    Ok(Discovery {
        crate_name,
        package_name,
        manifest_dir,
        target_dir,
        testrs_manifest_dir,
        dependencies,
        runtime_call,
        tear_downs,
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
    for td in &discovery.tear_downs {
        let asyncness = if td.is_async { " async" } else { "" };
        println!(
            "\n[tear_down]{asyncness} {}  (tears down: {:?})",
            td.call, td.subject
        );
    }
}
