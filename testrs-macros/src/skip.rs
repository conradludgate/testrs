//! The `#[skip(if = EXPR, reason = "...")]` attribute macro.
//!
//! `EXPR` is relocated into a generated `pub fn(<the test's parameters>) -> bool`
//! next to the test — so the condition sees the test's fixtures by name, is
//! type-checked against them (it must be a `bool`), and can reach their private
//! members (it lives in the user's crate, not the separate harness). A
//! `#[diagnostic::testrs::skip(cond = <predicate>, reason = "...")]` marker is
//! emitted pointing at it. At run time the harness evaluates the predicate with
//! the same arguments as the test; if it returns `true`, the test is reported
//! ignored rather than run. The sibling `#[test]` handles `pub`/dead-code, and
//! signature parsing is shared with `#[cases]` via [`crate::parse`].

// unsynn's combinator parsers return `Result<_, unsynn::Error>`, whose error is
// large; that's unsynn's type, not ours, and these parsers aren't on a hot path.
#![allow(clippy::result_large_err)]

use proc_macro2::{Literal, TokenStream, TokenTree};
use quote::{format_ident, quote};
// unsynn is designed to be glob-imported (combinators, operator names, traits).
#[allow(clippy::wildcard_imports)]
use unsynn::*;

use crate::parse::fn_name_and_params;

// `if` is a keyword; `reason` is a plain identifier we match by name.
keyword! { KwIf = "if"; KwReason = "reason"; }

unsynn! {
    /// `if = EXPR [, reason = "..."]`. The condition runs up to the optional
    /// `, reason = "..."` tail (top-level only — a comma inside a call or group is
    /// part of an atomic `TokenTree`, never the tail) or the end of the args.
    struct SkipArgs {
        _if: KwIf,
        _eq: Assign,
        cond: Many<Cons<Except<ReasonTail>, TokenTree>>,
        reason: Optional<ReasonTail>,
    }

    /// `, reason = "..."` — the optional reason tail.
    struct ReasonTail {
        _comma: Comma,
        _reason: KwReason,
        _eq: Assign,
        text: Literal,
    }
}

/// Expand `#[skip(if = EXPR, reason = "...")]` on a test `item`: relocate `EXPR`
/// into a `__testrs_skip_<fn>` predicate with the test's parameters and emit the
/// `#[diagnostic::testrs::skip(cond = ..., reason = ...)]` marker.
pub(crate) fn expand(attr: TokenStream, item: &TokenStream) -> TokenStream {
    let Ok(parsed) = attr.to_token_iter().parse_all::<SkipArgs>() else {
        return quote! {
            ::core::compile_error!(
                "#[skip] expects `if = <condition>` and an optional `reason = \"...\"`"
            );
            #item
        };
    };
    let cond = parsed.cond.to_token_stream();
    // Default the reason to the condition's source text when one isn't given.
    let reason = match parsed.reason.into_iter().next() {
        Some(tail) => tail.value.text,
        None => Literal::string(&cond.to_string()),
    };

    let Some((fn_name, params)) = fn_name_and_params(item) else {
        return quote! { #item };
    };
    let predicate = format_ident!("__testrs_skip_{}", fn_name);
    let sig_params: Vec<TokenStream> = params
        .iter()
        .map(|(pattern, ty)| quote! { #pattern: #ty })
        .collect();

    quote! {
        #[diagnostic::testrs::skip(cond = #predicate, reason = #reason)]
        #item
        #[doc(hidden)]
        #[allow(unused_variables)]
        pub fn #predicate(#(#sig_params),*) -> bool { #cond }
    }
}
