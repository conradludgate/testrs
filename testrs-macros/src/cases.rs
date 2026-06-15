//! The `#[cases(param = EXPR, ...)]` attribute macro.
//!
//! Each `EXPR` is relocated into a generated `pub fn() -> impl IntoIterator<Item =
//! T>` next to the test (`T` is the parameter's type with its leading `&`
//! stripped), and a `#[diagnostic::testrs::cases(...)]` marker is emitted pointing
//! at those providers. The return annotation does three jobs: it lets `EXPR` be
//! any `IntoIterator`, it names the element type so the (separate) harness crate
//! can collect it, and it type-checks `EXPR` against the parameter. The sibling
//! `#[test]` handles `pub`/dead-code promotion, so this macro doesn't. The
//! grammars below are parsed with unsynn.

// unsynn's combinator parsers return `Result<_, unsynn::Error>`, whose error is
// large; that's unsynn's type, not ours, and these parsers aren't on a hot path.
#![allow(clippy::result_large_err)]

use std::collections::HashMap;

use proc_macro2::{Delimiter, Group, Ident, Span, TokenStream, TokenTree};
use quote::{format_ident, quote};
// unsynn is designed to be glob-imported (combinators, operator names, traits).
#[allow(clippy::wildcard_imports)]
use unsynn::*;

/// Expand `#[cases(param = EXPR, ...)]` on a test `item`: generate the providers,
/// body-inject the param references (so they resolve in an editor), and emit the
/// `#[diagnostic::testrs::cases(param = provider, ...)]` marker.
pub(crate) fn expand(attr: TokenStream, item: TokenStream) -> TokenStream {
    let bindings = cases_bindings(attr);
    if bindings.is_empty() {
        return quote! { #[diagnostic::testrs::cases] #item };
    }

    let (fn_name, param_types) = parse_signature(&item);
    let mut providers = TokenStream::new();
    let mut rewrites = Vec::new();
    let mut param_refs = TokenStream::new();
    for (param, expr) in &bindings {
        // Resolve the param name to the test parameter (in scope in the body).
        param_refs.extend(quote! { let _ = &#param; });
        let Some(elem) = param_types.get(&param.to_string()) else {
            continue; // a typo'd param name; the ref above gives a clear error
        };
        let provider = format_ident!("__testrs_cases_{}_{}", fn_name, param);
        providers.extend(quote! {
            #[doc(hidden)]
            pub fn #provider() -> impl ::core::iter::IntoIterator<Item = #elem> { #expr }
        });
        rewrites.push((param.clone(), provider));
    }

    let rewritten: Vec<TokenStream> = rewrites
        .iter()
        .map(|(param, provider)| quote! { #param = #provider })
        .collect();
    let item = inject_into_body(item, param_refs);
    quote! {
        #[diagnostic::testrs::cases(#(#rewritten),*)]
        #item
        #providers
    }
}

// The `fn` keyword, for the signature grammar.
keyword! { KwFn = "fn"; }

// Grammars for the parts of `#[test(cases(...))]` we parse with unsynn. Each item
// ends with an optional `,` separator (a trailing comma is stripped first, see
// `parse_items`), so items split without a dangling delimiter.
unsynn! {
    /// `param = EXPR`. The expression runs up to the start of the next binding — a
    /// comma followed by `IDENT =`. A comma *not* followed by that (e.g. inside a
    /// turbofish `::<A, B>`) is consumed as part of the expression, not a separator.
    struct CaseBinding {
        param: Ident,
        _eq: Assign,
        expr: Many<Cons<Except<NextBinding>, TokenTree>>,
        _sep: Optional<Comma>,
    }

    /// Look-ahead for the start of the next binding: `, IDENT =`.
    struct NextBinding {
        _comma: Comma,
        _param: Ident,
        _eq: Assign,
    }

    /// `pattern: TYPE` — one function parameter. unsynn matches the separating `:`
    /// (a lone `Colon`, distinct from a path's `::`); testrs forbids generic
    /// parameter types, so a plain comma always ends the type.
    struct SigParam {
        pattern: Many<Cons<Except<Colon>, TokenTree>>,
        _colon: Colon,
        ty: Many<Cons<Except<Comma>, TokenTree>>,
        _sep: Optional<Comma>,
    }

    /// `… fn NAME(params) …` — skip attributes / visibility / qualifiers up to the
    /// `fn` keyword, then capture the name and parameter list. testrs tests aren't
    /// generic, so the parameters follow the name directly.
    struct FnHeader {
        _prefix: Many<Cons<Except<KwFn>, TokenTree>>,
        _fn: KwFn,
        name: Ident,
        params: ParenthesisGroupContaining<Vec<SigParam>>,
    }
}

/// Extract `(param, expr)` from each `param = EXPR` in the `#[cases(...)]` args.
fn cases_bindings(attr: TokenStream) -> Vec<(Ident, TokenStream)> {
    parse_items::<CaseBinding>(attr)
        .into_iter()
        .map(|b| (b.param, b.expr.to_token_stream()))
        .collect()
}

/// The test's name and a map from parameter name to its *element* type tokens
/// (the parameter type with a leading `&`/lifetime stripped).
fn parse_signature(item: &TokenStream) -> (Ident, HashMap<String, TokenStream>) {
    let mut types = HashMap::new();
    let mut iter = item.clone().to_token_iter();
    let Ok(header) = FnHeader::parse(&mut iter) else {
        return (Ident::new("unknown", Span::call_site()), types);
    };
    for param in header.params.content {
        // The binding name is the last ident of the pattern (skips `mut`/`ref`).
        if let Some(name) = last_ident(&param.pattern.to_token_stream()) {
            types.insert(name, element_type(param.ty.to_token_stream()));
        }
    }
    (header.name, types)
}

/// Parse the comma-separated `T`s of a token stream. A trailing comma is stripped
/// first so the final item parses like the others (each `T` ends with an
/// `Optional<Comma>`); an empty or unparseable stream yields no items.
fn parse_items<T: Parse>(inner: TokenStream) -> Vec<T> {
    let mut toks: Vec<TokenTree> = inner.into_iter().collect();
    if matches!(toks.last(), Some(TokenTree::Punct(p)) if p.as_char() == ',') {
        toks.pop();
    }
    if toks.is_empty() {
        return Vec::new();
    }
    toks.into_iter()
        .collect::<TokenStream>()
        .to_token_iter()
        .parse_all::<Vec<T>>()
        .unwrap_or_default()
}

/// The last identifier in a token stream — a parameter pattern's binding name.
fn last_ident(ts: &TokenStream) -> Option<String> {
    ts.clone()
        .into_iter()
        .filter_map(|tt| match tt {
            TokenTree::Ident(id) => Some(id.to_string()),
            _ => None,
        })
        .last()
}

/// Strip a leading `&`, optional lifetime, and optional `mut` from a parameter
/// type, leaving the element type (`&Vector` → `Vector`).
fn element_type(ty: TokenStream) -> TokenStream {
    let toks: Vec<TokenTree> = ty.into_iter().collect();
    let mut i = 0;
    if matches!(toks.first(), Some(TokenTree::Punct(p)) if p.as_char() == '&') {
        i += 1;
        if matches!(toks.get(i), Some(TokenTree::Punct(p)) if p.as_char() == '\'') {
            i += 1;
            if matches!(toks.get(i), Some(TokenTree::Ident(_))) {
                i += 1;
            }
        }
        if matches!(toks.get(i), Some(TokenTree::Ident(id)) if *id == "mut") {
            i += 1;
        }
    }
    toks[i..].iter().cloned().collect()
}

/// Prepend statements to a function's body — the last top-level brace group of the
/// item's tokens. If there's no body (no brace group), the item is unchanged.
fn inject_into_body(item: TokenStream, stmts: TokenStream) -> TokenStream {
    let mut tokens: Vec<TokenTree> = item.into_iter().collect();
    let body = tokens
        .iter()
        .rposition(|tt| matches!(tt, TokenTree::Group(g) if g.delimiter() == Delimiter::Brace));
    if let Some(i) = body
        && let TokenTree::Group(g) = &tokens[i]
    {
        let mut inner = stmts;
        inner.extend(g.stream());
        let mut new_body = Group::new(Delimiter::Brace, inner);
        new_body.set_span(g.span());
        tokens[i] = TokenTree::Group(new_body);
    }
    tokens.into_iter().collect()
}
