//! Signature parsing shared by the `#[cases]` and `#[skip]` macros.
//!
//! Both need to read a test's parameter list: `#[cases]` to name each parameter's
//! element type, `#[skip]` to give its generated predicate the same parameters as
//! the test. The unsynn grammars and helpers live here so neither macro re-derives
//! them.

// unsynn's combinator parsers return `Result<_, unsynn::Error>`, whose error is
// large; that's unsynn's type, not ours, and these parsers aren't on a hot path.
#![allow(clippy::result_large_err)]

use proc_macro2::{Ident, TokenStream, TokenTree};
// unsynn is designed to be glob-imported (combinators, operator names, traits).
#[allow(clippy::wildcard_imports)]
use unsynn::*;

// The `fn` keyword, for the signature grammar.
keyword! { KwFn = "fn"; }

unsynn! {
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

/// The test's name and its parameters as `(pattern, type)` token pairs, verbatim.
/// Returns `None` if the item has no parseable `fn` header.
pub(crate) fn fn_name_and_params(
    item: &TokenStream,
) -> Option<(Ident, Vec<(TokenStream, TokenStream)>)> {
    let mut iter = item.clone().to_token_iter();
    let header = FnHeader::parse(&mut iter).ok()?;
    let params = header
        .params
        .content
        .into_iter()
        .map(|p| (p.pattern.to_token_stream(), p.ty.to_token_stream()))
        .collect();
    Some((header.name, params))
}

/// Parse the comma-separated `T`s of a token stream. A trailing comma is stripped
/// first so the final item parses like the others (each `T` ends with an
/// `Optional<Comma>`); an empty or unparseable stream yields no items.
pub(crate) fn parse_items<T: Parse>(inner: TokenStream) -> Vec<T> {
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
pub(crate) fn last_ident(ts: &TokenStream) -> Option<String> {
    ts.clone()
        .into_iter()
        .filter_map(|tt| match tt {
            TokenTree::Ident(id) => Some(id.to_string()),
            _ => None,
        })
        .last()
}
