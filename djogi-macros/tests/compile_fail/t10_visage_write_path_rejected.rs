//! visage write-path rejection.
//!
//! Visages are read-only projections. Calling a write method
//! (`bulk_create`, `save`, `delete`) on a visage type fails with a
//! "no function or associated item" error because the macro emits no
//! such methods on the visage struct. No additional trait plumbing is
//! needed — method absence is the compile-time enforcement.
//!
//! This fixture probes `bulk_create` specifically; `save` / `delete`
//! follow the same shape and fail with the same diagnostic class.

use djogi::prelude::*;

#[model(table = "x")]
#[derive(Debug, Clone)]
pub struct X {
    #[field(expose(public))]
    pub name: String,
}

async fn _probe_visage_write_path(ctx: &mut DjogiContext) {
    // Visage queries are read-only — `bulk_create` is not emitted on
    // the visage struct. The compiler reports it as missing. The
    // function form (vs an `async {}` block with `todo!()`) avoids the
    // `unreachable_code` warning that fires after a diverging
    // expression, which would otherwise drift the stored stderr.
    let _ = XPublic::bulk_create(ctx, ::std::vec![]).await;
}

fn main() {}
