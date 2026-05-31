//! `djogi::djogi_main!(Model1, Model2, …)` function-like proc macro.
//!
//! Generates a `fn main()` that references the listed model types to
//! prevent the LTO linker from dropping their crates' inventory data,
//! then delegates to `djogi_cli::run_from_env()`.
//!
//! The linker spike (REQ-370-SPIKE-0) proved that referencing a single
//! descriptor per crate forces ALL inventory from that crate. This macro
//! makes that reference explicit and auditable at the binary entry point.
//!
//! # Usage
//!
//! ```ignore
//! djogi::djogi_main!(tracker::Elephant, billing::Invoice);
//! ```
//!
//! Expands to:
//!
//! ```ignore
//! fn main() -> std::process::ExitCode {
//!     let _ = <tracker::Elephant as ::djogi::model::Model>::descriptor();
//!     let _ = <billing::Invoice as ::djogi::model::Model>::descriptor();
//!     ::djogi_cli::run_from_env()
//! }
//! ```

use proc_macro2::TokenStream;
use quote::quote;
use syn::Path;
use syn::parse::{Parse, ParseStream};

/// Parse a comma-separated list of type paths.
///
/// Accepts zero or more paths separated by commas. Trailing commas
/// are permitted so `djogi_main!(Elephant,)` is valid.
struct ModelPaths {
    paths: Vec<Path>,
}

impl Parse for ModelPaths {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut paths = Vec::new();
        while !input.is_empty() {
            paths.push(input.parse::<Path>()?);
            if input.is_empty() {
                break;
            }
            // Consume the comma separator (trailing comma is fine).
            input.parse::<syn::Token![,]>()?;
        }
        Ok(ModelPaths { paths })
    }
}

/// Expand `djogi_main!(Model1, Model2, …)` into a `fn main()` that
/// forces all listed model crates into the linkage graph and delegates
/// to `djogi_cli::run_from_env()`.
pub fn djogi_main(input: TokenStream) -> TokenStream {
    let ModelPaths { paths } = match syn::parse2::<ModelPaths>(input) {
        Ok(p) => p,
        Err(e) => return e.to_compile_error(),
    };

    let refs: Vec<_> = paths
        .iter()
        .map(|path| {
            quote! {
                // Forces `inventory::submit!` from this type's crate into the binary.
                // Without this reference, LTO/linker may drop the entire crate.
                let _ = <#path as ::djogi::model::Model>::descriptor();
            }
        })
        .collect();

    quote! {
        fn main() -> std::process::ExitCode {
            // Force all model crates into the linkage graph so inventory data survives LTO.
            #(#refs)*
            ::djogi_cli::run_from_env()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expand_single_model() {
        let input: TokenStream = quote! { tracker::Elephant };
        let out = djogi_main(input);
        let s = out.to_string();

        // Must emit a valid main function.
        assert!(s.contains("fn main"), "output should contain 'fn main'");
        assert!(
            s.contains("djogi :: model :: Model"),
            "output should reference djogi::model::Model trait (not derive macro shadow)"
        );
        assert!(
            s.contains("descriptor"),
            "output should reference the descriptor associated fn"
        );
        assert!(
            s.contains("tracker :: Elephant"),
            "output should contain the model path"
        );
        assert!(
            s.contains("djogi_cli :: run_from_env"),
            "output should call djogi_cli::run_from_env()"
        );
        // Return type must match run_from_env() -> ExitCode
        assert!(
            s.contains("ExitCode"),
            "output should return std::process::ExitCode, got: {s}"
        );
    }

    #[test]
    fn test_expand_multiple_models() {
        let input: TokenStream = quote! { tracker::Elephant , billing::Invoice };
        let out = djogi_main(input);
        let s = out.to_string();

        assert!(s.contains("fn main"), "output should contain 'fn main'");
        assert!(
            s.contains("tracker :: Elephant"),
            "output should contain first model path"
        );
        assert!(
            s.contains("billing :: Invoice"),
            "output should contain second model path"
        );
        // Both models should have descriptor references.
        let descriptor_count = s.matches("descriptor").count();
        assert_eq!(
            descriptor_count, 2,
            "output should have two descriptor references, got: {s}"
        );
        // Return type must be ExitCode
        assert!(
            s.contains("ExitCode"),
            "output should return std::process::ExitCode, got: {s}"
        );
    }

    #[test]
    fn test_expand_no_models() {
        // Empty input (no tokens) — still produces a valid main.
        let input: TokenStream = TokenStream::new();
        let out = djogi_main(input);
        let s = out.to_string();

        assert!(s.contains("fn main"), "output should contain 'fn main'");
        assert!(
            s.contains("djogi_cli :: run_from_env"),
            "output should call djogi_cli::run_from_env() even with no models"
        );
        // No descriptor references when no models are listed.
        assert!(
            !s.contains("descriptor"),
            "output should not contain descriptor refs for empty input, got: {s}"
        );
    }

    #[test]
    fn test_no_unsafe_in_output() {
        // G6 forbid-unsafe compatibility: expansion must contain no unsafe tokens.
        let input: TokenStream = quote! { tracker::Elephant };
        let out = djogi_main(input);
        let s = out.to_string();
        assert!(
            !s.contains("unsafe"),
            "djogi_main! expansion must be unsafe-free for #![forbid(unsafe_code)] compat, got: {s}"
        );
    }
}
