//! Phase 8α T2.2 integration tests: `#[derive(Auditable)]` proc macro.
//!
//! What this file pins:
//!
//! 1. `#[derive(Auditable)]` emits an `impl ::djogi::Auditable for #ident`
//!    block whose `created_by()` getter returns `Option<&str>` borrowed
//!    from the adopter-declared `created_by: Option<String>` field.
//! 2. The derive does **not** inject the field — the adopter declares it
//!    explicitly (Path B per Phase 8 v3 line 866). If the field is
//!    missing, the emitted impl fails to compile. T2.5 may add a
//!    compile_fail trybuild fixture; this file exercises the success
//!    path.
//! 3. The `Auditable` trait is convention-sealed only; the derive routes
//!    the impl through the public `::djogi::Auditable` re-export, not
//!    through `::djogi::__private::*`.
//!
//! # One model per test — coherence
//!
//! `impl Auditable for T` is a coherent impl: only one per `T` per crate.
//! Each test therefore declares its own model type sharing a single
//! `audit_*` table shape.
//!
//! # Fixture strategy
//!
//! Each test provisions its own table inline via `ctx.raw_execute(...)`.
//! `#[djogi::djogi_test]` already installs HeeRanjID schema, seeds node
//! 1, and sets `heer.node_id = '1'` before the test body runs.

use djogi::Auditable;
use djogi::prelude::*;

// ---------------------------------------------------------------------------
// Test 1 — `created_by()` returns `Some(&str)` borrowed from the
// in-memory `created_by: Option<String>` field.
// ---------------------------------------------------------------------------

#[derive(Auditable)]
#[model(table = "audit_present")]
#[derive(Debug, Clone)]
pub struct AuditPresent {
    pub note: String,
    pub created_by: Option<String>,
}

async fn setup_audit_present(ctx: &mut djogi::DjogiContext) {
    ctx.raw_execute(
        "CREATE TABLE audit_present (
            id          BIGINT      PRIMARY KEY DEFAULT generate_id(),
            created_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            note        TEXT        NOT NULL,
            created_by  TEXT
        )",
        &[],
    )
    .await
    .expect("create audit_present table");
}

#[djogi::djogi_test]
async fn auditable_getter_returns_created_by(mut ctx: djogi::DjogiContext) {
    setup_audit_present(&mut ctx).await;

    let row = AuditPresent::create(
        &mut ctx,
        AuditPresent {
            note: "first".into(),
            created_by: Some("alice".into()),
            ..Default::default()
        },
    )
    .await
    .expect("create should succeed");

    // The trait-method call exercises the macro-emitted impl. The
    // `as_deref()` body returns a borrowed `&str` pointing into the
    // in-memory String — no allocation, no copy.
    assert_eq!(
        row.created_by(),
        Some("alice"),
        "Auditable::created_by() must return the adopter-declared field as Option<&str>",
    );

    // Confirm the bound is usable at the framework boundary: code that
    // wants to talk generically about "models with audit metadata" can
    // accept any `M: djogi::Auditable`. If T2.2 emitted the wrong path
    // (e.g. routed through `__private`), this generic call would still
    // compile but the public `djogi::Auditable` import at the top of the
    // file would be unused — clippy's `unused_imports` lint catches that
    // separately under the workspace `-D warnings`.
    fn read_user<M: djogi::Auditable>(m: &M) -> Option<String> {
        m.created_by().map(str::to_owned)
    }
    assert_eq!(read_user(&row), Some("alice".to_owned()));
}

// ---------------------------------------------------------------------------
// Test 2 — `created_by()` returns `None` when the field is `None`.
// ---------------------------------------------------------------------------

#[derive(Auditable)]
#[model(table = "audit_absent")]
#[derive(Debug, Clone)]
pub struct AuditAbsent {
    pub note: String,
    pub created_by: Option<String>,
}

async fn setup_audit_absent(ctx: &mut djogi::DjogiContext) {
    ctx.raw_execute(
        "CREATE TABLE audit_absent (
            id          BIGINT      PRIMARY KEY DEFAULT generate_id(),
            created_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            note        TEXT        NOT NULL,
            created_by  TEXT
        )",
        &[],
    )
    .await
    .expect("create audit_absent table");
}

#[djogi::djogi_test]
async fn created_by_returns_none_when_unset(mut ctx: djogi::DjogiContext) {
    setup_audit_absent(&mut ctx).await;

    let row = AuditAbsent::create(
        &mut ctx,
        AuditAbsent {
            note: "second".into(),
            created_by: None,
            ..Default::default()
        },
    )
    .await
    .expect("create should succeed");

    assert_eq!(
        row.created_by(),
        None,
        "Auditable::created_by() must return None when the column is NULL — \
         no warn-on-null per Phase 8 §D6 lines 149-157",
    );
}
