//! `djogi::link_anchor!(ModelType)` function-like proc macro.
//!
//! Emits a `#[used]` static that references the given model's
//! descriptor, forcing the entire crate into the linkage graph even
//! under aggressive LTO.
//!
//! This is the **per-crate fallback** for when `djogi_main!` cannot be
//! used (e.g., the model lives in a library crate without a `main()`).
//! Call from any `fn main()` or from a module that is guaranteed to be
//! linked into the final binary.
//!
//! # Usage
//!
//! ```ignore
//! // In each model crate's lib.rs:
//! djogi::link_anchor!(MyModel);
//! ```

use proc_macro2::TokenStream;
use syn::Path;
use syn::parse::{Parse, ParseStream};

/// Parse a single type path.
struct SinglePath(Path);

impl Parse for SinglePath {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let path = input.parse::<Path>()?;
        if !input.is_empty() {
            return Err(input.error("expected a single type path"));
        }
        Ok(SinglePath(path))
    }
}

/// Expand `link_anchor!(ModelType)` into a `#[used]` static that
/// references the model's descriptor to prevent LTO from dropping
/// the crate from the linkage graph.
pub fn link_anchor(input: TokenStream) -> TokenStream {
    let SinglePath(model_path) = match syn::parse2::<SinglePath>(input) {
        Ok(p) => p,
        Err(e) => return e.to_compile_error(),
    };

    quote::quote! {
        /// Forces the inventory data from this crate into the linkage graph.
        /// Call from `main()` before any djogi operations to prevent LTO
        /// from dropping model descriptors.
        #[used]
        pub static __DJOGI_LINK_ANCHOR: unsafe extern "C" fn() = {
            // Reference prevents dead-code elimination of the descriptor.
            let _ = <#model_path as ::djogi::Model>::descriptor;
            #[no_mangle]
            unsafe extern "C" fn anchor() {}
            anchor
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expand_single_model() {
        let input: TokenStream = quote::quote! { MyModel };
        let out = link_anchor(input);
        let s = out.to_string();

        assert!(
            s.contains("__DJOGI_LINK_ANCHOR"),
            "output should contain the static name, got: {s}"
        );
        assert!(
            s.contains("djogi :: Model"),
            "output should reference djogi::Model trait"
        );
        assert!(
            s.contains("descriptor"),
            "output should reference descriptor"
        );
        assert!(
            s.contains("MyModel"),
            "output should contain the model name"
        );
        assert!(
            s.contains("# [used]"),
            "output should have #[used] attribute to prevent LTO stripping, got: {s}"
        );
    }

    #[test]
    fn test_expand_nested_path() {
        let input: TokenStream = quote::quote! { foo::bar::Baz };
        let out = link_anchor(input);
        let s = out.to_string();

        assert!(
            s.contains("foo :: bar :: Baz"),
            "output should preserve the full nested path, got: {s}"
        );
        assert!(
            s.contains("__DJOGI_LINK_ANCHOR"),
            "output should contain the static name"
        );
    }
}
