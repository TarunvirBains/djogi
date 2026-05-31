//! `djogi::link_anchor!()` per-crate linkage anchor (#370, branch b).
//!
//! When referencing ONE model's `descriptor()` does NOT retain a crate's
//! sibling models under the CI release profile (`--gc-sections` + multiple
//! codegen units split sibling `submit!` statics into objects the linker
//! never pulls), the robust fallback is a single dedicated anchor symbol
//! per crate. Each model crate invokes `djogi::link_anchor!()` ONCE in its
//! `lib.rs`; the adopter glue references `<crate>::__djogi_link_anchor()`
//! once per crate. Referencing that one symbol pulls the crate's rlib
//! member into the binary, and `inventory`'s registration statics (already
//! emitted by `#[derive(Model)]`) are collected for the whole linked crate.
//!
//! Takes NO arguments — it is a per-crate marker, not per-model. A non-empty
//! invocation is a compile error.
//!
//! The expansion contains zero `unsafe` tokens — compatible with
//! `#![forbid(unsafe_code)]` (category G6).
//!
//! # Usage
//!
//! ```ignore
//! // In each model crate's lib.rs, once:
//! djogi::link_anchor!();
//!
//! // In the adopter's src/bin/djogi.rs, one reference per model crate:
//! fn main() -> std::process::ExitCode {
//!     tracker::__djogi_link_anchor();
//!     billing::__djogi_link_anchor();
//!     djogi_cli::run_from_env()
//! }
//! ```

use proc_macro2::TokenStream;
use quote::quote;

/// Expand `link_anchor!()` — emit one per-crate anchor symbol.
///
/// Takes NO arguments (it is a per-crate marker, not per-model — that is
/// the whole point: ONE invocation covers all of a crate's models). A
/// non-empty invocation is a compile error.
pub fn link_anchor(input: TokenStream) -> TokenStream {
    if !input.is_empty() {
        return syn::Error::new(
            proc_macro2::Span::call_site(),
            "djogi::link_anchor! takes no arguments — invoke it once per model \
             crate's lib.rs as `djogi::link_anchor!();`.",
        )
        .to_compile_error();
    }

    // A single, uniquely-pathed (via the crate root) anchor symbol. The
    // adopter glue calls `<crate>::__djogi_link_anchor()` once per crate;
    // that call is the external reference that forces the crate's rlib
    // member into the binary, and the crate's `inventory` statics (emitted
    // by #[derive(Model)] with #[used] + a linker section) are then
    // collected for the whole linked crate.
    //
    // `#[used]` lives on the STATIC `__DJOGI_LINK_ANCHOR`, not on the fn —
    // `#[used]` is a static-only attribute (rustc rejects it on a fn,
    // E0518), and it is precisely how `inventory` itself defeats
    // `--gc-sections` (it tags its `static __CTOR` `#[used]`; see
    // `inventory`'s `__do_submit!`). The static carries the dead-strip
    // defense; the `pub fn` is the callable surface the adopter references.
    // The fn returns a reference to the static so the static cannot be
    // dropped independently of a fn that is kept, and the fn's body forces
    // the `#[used]` static to participate. No `unsafe` tokens — the static
    // is a plain `()` (G6 / forbid-unsafe-safe).
    // `#[doc(hidden)]` — adopters reference it only through the documented
    // glue, not as public API. `#[inline(never)]` keeps the fn a real
    // callable symbol the reference cannot be optimized away to nothing
    // before the crate is pulled.
    quote! {
        #[doc(hidden)]
        #[used]
        static __DJOGI_LINK_ANCHOR: () = ();

        #[doc(hidden)]
        #[inline(never)]
        pub fn __djogi_link_anchor() -> &'static () {
            &__DJOGI_LINK_ANCHOR
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_emits_anchor() {
        let out = link_anchor(TokenStream::new());
        let s = out.to_string();
        assert!(
            s.contains("__djogi_link_anchor"),
            "must emit the anchor fn: {s}"
        );
        // The dead-strip defense is `#[used]` on the anchor static — the
        // doc + commit claim this, so the test pins it (the prior impl
        // omitted it; doc-impl drift, codex plan-review pass-2 #2). The
        // tokenized attribute renders as `# [used]`, so normalize spaces
        // before matching rather than asserting the source spelling.
        let collapsed: String = s.split_whitespace().collect();
        assert!(
            collapsed.contains("#[used]"),
            "anchor must carry #[used] (the --gc-sections dead-strip defense): {s}"
        );
        assert!(
            !s.contains("unsafe"),
            "anchor must be unsafe-free (G6 / forbid-unsafe compat): {s}"
        );
    }

    #[test]
    fn nonempty_input_is_compile_error() {
        let out = link_anchor(quote! { Foo });
        assert!(
            out.to_string().contains("compile_error"),
            "args imply compile_error!"
        );
    }
}
