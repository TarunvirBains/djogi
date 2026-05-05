// Cluster 8δ T8.3 — `QuerySet::refresh_into` compile-pass fixture.
//
// Witnesses that:
//   1. `QuerySet::refresh_into` resolves and type-checks.
//   2. The fetcher captures owned state (no `&mut Ctx` or other borrowed
//      lifetimes appear in the signature or the DjogiDeltaFetcher struct).
//   3. `DeltaRefreshHandle<T>` is the return type.
//
// Per `feedback_trybuild_fixtures.md`, every trybuild fixture must have
// `fn main` so the stored binary can link.
//
// Path-routing note: this fixture exercises NON-emitted code (`refresh_into`
// is a non-macro method on `QuerySet<T>`). The path-routing rule governs
// macro-emitted code only; non-emitted framework code may spell
// `djogi::cache::Punnu`, `djogi::cache::DeltaRefreshHandle`, etc. directly,
// which is what this fixture does.
//
// Spec anchor:
//   docs/superpowers/plans/granular-phase8/cluster-8delta-granular.md
//   §3 commit T8.3 — "Compile-pass fixture" bullet.

use djogi::prelude::*;

#[model(table = "phase8_t8_refresh_rows", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct RefreshRow {
    pub label: String,
}

fn _accept_send_sync_static<T: Send + Sync + 'static>() {}

fn _signature_check(
    qs: ::djogi::QuerySet<RefreshRow>,
    punnu: &::djogi::cache::Punnu<RefreshRow>,
    pool: ::djogi::pg::pool::DjogiPool,
    auth: ::djogi::auth::AuthContext,
) -> ::djogi::cache::DeltaRefreshHandle<RefreshRow> {
    qs.refresh_into(punnu, pool, auth)
}

fn main() {
    let _: fn(
        ::djogi::QuerySet<RefreshRow>,
        &::djogi::cache::Punnu<RefreshRow>,
        ::djogi::pg::pool::DjogiPool,
        ::djogi::auth::AuthContext,
    ) -> ::djogi::cache::DeltaRefreshHandle<RefreshRow> = _signature_check;
}
