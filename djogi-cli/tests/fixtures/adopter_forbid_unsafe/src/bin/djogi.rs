//! #370 T-FORBID-UNSAFE: `djogi_main!` compiles under
//! `#![forbid(unsafe_code)]` — the macro expansion contains no
//! unsafe tokens.
#![forbid(unsafe_code)]
djogi_cli::djogi_main!(adopter_forbid_unsafe::Thing);
