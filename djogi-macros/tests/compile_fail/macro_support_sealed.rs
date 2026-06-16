//! An adopter crate cannot call the macro-support execution helpers on
//! `DjogiContext` directly. These helpers moved off the inherent surface
//! onto the sealed `MacroSupportExt` trait, which adopter code does not
//! (and should not) import. A direct call must fail to resolve.
//!
//! GitHub: djogi#433.

use djogi::DjogiContext;

async fn hostile_direct_call(ctx: &mut DjogiContext) {
    // No `use djogi::__private::MacroSupportExt;` here on purpose: an
    // adopter would never write that. Without it, the method is not in
    // scope and the call must not resolve.
    let _ = ctx.__query_all_for_macros("SELECT 1", &[]).await;
}

fn main() {}
