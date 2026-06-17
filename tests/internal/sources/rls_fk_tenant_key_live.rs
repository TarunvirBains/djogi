// / .1 — Live integration test: tenant_key + ForeignKey<T> RLS
//
// Regression test for [GH issue #37]. / already cover
// tenant isolation when `tenant_key` names a plain `i64` column (see
// `postgres_native::set_tenant_rls_isolates_tenants`). What that
// test does NOT cover is the case where `tenant_key` names a
// `ForeignKey<Owner>`-typed column — the surface where #37 originally
// manifested.
//
// Pre-fix (issue #37) the macro path that emits the side-channel RLS
// DDL fell through `field_sql_type_category` to the `Unsupported` arm
// when it saw `ForeignKey<Owner>`, producing `cast_suffix = ""`. The
// emitted policy expression was therefore
// `owner_id = current_setting('app.tenant_id', true)` — comparing a
// BIGINT column against an unanounced TEXT, which Postgres treats as
// NULL after coercion failure, hiding every row regardless of the
// current tenant. Adopters who relied on the macro-emitted policy got
// a silent-correctness footgun: tests passed (zero rows visible looks
// like clean isolation), production hid all tenant data.
//
// This live test asserts the post-fix shape end-to-end:
//
// 1. **`rls_with_fk_tenant_key_filters_correctly`** — set up two
//    `Owner` rows (1000, 2000), give each one `Document`, then under
//    a restricted role apply `set_tenant("1000")` and assert exactly
//    one row is visible (the 1000-owned doc); switch to
//    `set_tenant("2000")` and assert exactly the other doc shows. The
//    pre-fix behaviour returns 0 rows for both — the cast against an
//    untyped `current_setting(...)` value compared to a BIGINT column
//    fails and Postgres hides everything.
//
// 2. **`rls_policy_ddl_uses_bigint_cast`** — read the macro-emitted
//    side-channel `target/djogi_rls/{table}_rls.sql` file and assert
//    its `USING (...)` clause contains the literal `::bigint` cast
//    suffix matching the FK inner PK type (HeerId → BIGINT). This is
//    the document-level proof the macro path that decides the cast
//    correctly unwrapped `ForeignKey<Owner>` to its inner `HeerId`
//    type.
//
// Both tests run in `#[djogi::djogi_test]` which provisions a per-test
// database, so they are mutually independent and parallel-safe. The
// restricted role `djogi_rls_test_user` is  and shared
// with `postgres_native::set_tenant_rls_isolates_tenants` —
// the idempotent CREATE ROLE pattern from that test is replicated
// here so the suites can run in any order.
//
// [GH issue #37]: https://github.com/tarunvirbains/djogi/issues/37

use djogi::prelude::*;
use djogi_macros::model;

#[model(table = "owners", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Owner {
    pub name: String,
}

/// The model under test. `tenant_key = "owner_id"` where `owner_id` is
/// `ForeignKey<Owner>` — pre-fix this combination produced an empty
/// `::cast` suffix; post-fix it routes through `BigInt` and emits
/// `::bigint`.
#[model(
    table = "documents",
    pk = HeerId,
    tenant_key = "owner_id",
    no_default
)]
#[derive(Debug, Clone)]
pub struct Document {
    pub owner_id: ForeignKey<Owner>,
    pub title: String,
}

/// Idempotent table + policy + restricted-role bootstrap.
///
/// The role `djogi_rls_test_user` is  (Postgres roles are
/// not per-database). Repeated test runs across the same cluster reuse
/// it; the existence-then-create idiom matches the pattern in
/// `postgres_native::set_tenant_rls_isolates_tenants`.
async fn setup(ctx: &mut djogi::DjogiContext) {
    // `#[djogi_test]` provisions a fresh DB, so no DROP guards needed.

    ctx.raw_execute(
        "CREATE TABLE owners (
             id          BIGINT PRIMARY KEY DEFAULT generate_id(),
             created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
             updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
             name        TEXT NOT NULL
         )",
        &[],
    )
    .await
    .expect("create owners");

    ctx.raw_execute(
        "CREATE TABLE documents (
             id          BIGINT PRIMARY KEY DEFAULT generate_id(),
             created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
             updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
             owner_id    BIGINT NOT NULL REFERENCES owners(id),
             title       TEXT   NOT NULL
         )",
        &[],
    )
    .await
    .expect("create documents");

    ctx.raw_execute(
        "ALTER TABLE documents ENABLE ROW LEVEL SECURITY",
        &[],
    )
    .await
    .expect("enable RLS");

    // FORCE so the policy applies even to non-superuser table owners.
    // SET LOCAL ROLE below switches to a restricted role anyway, since
    // superusers always bypass RLS regardless of FORCE.
    ctx.raw_execute(
        "ALTER TABLE documents FORCE ROW LEVEL SECURITY",
        &[],
    )
    .await
    .expect("force RLS");

    // ::bigint is the literal post-fix cast: `ForeignKey<Owner>` →
    // BigInt → "::bigint". Shipping the policy DDL hand-written in the
    // test mirrors what the macro emits to `target/djogi_rls/`. The
    // companion test `rls_policy_ddl_uses_bigint_cast` verifies the
    // emitted file matches.
    ctx.raw_execute(
        "CREATE POLICY documents_tenant_isolation ON documents \
         USING (owner_id = current_setting('app.tenant_id', true)::bigint)",
        &[],
    )
    .await
    .expect("create RLS policy");

    // Cluster-level role — idempotent.
    let role_exists: bool = ctx
        .raw_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'djogi_rls_test_user')",
            &[],
        )
        .await
        .expect("check role");
    if !role_exists {
        ctx.raw_ddl("CREATE ROLE djogi_rls_test_user")
            .await
            .expect("create role");
    }

    // The restricted role needs SELECT on owners (to satisfy the FK
    // existence check during INSERT), and SELECT on documents to
    // exercise the policy.
    ctx.raw_execute(
        "GRANT SELECT ON owners TO djogi_rls_test_user",
        &[],
    )
    .await
    .expect("grant owners");
    ctx.raw_execute(
        "GRANT SELECT ON documents TO djogi_rls_test_user",
        &[],
    )
    .await
    .expect("grant documents");
    ctx.raw_execute("GRANT USAGE ON SCHEMA public TO djogi_rls_test_user", &[])
        .await
        .expect("grant schema");
}

/// Open a fresh `atomic()` scope under the restricted RLS-test role,
/// apply `set_tenant(tenant)`, fetch all `Document` rows, and project
/// to titles. Each invocation gets its own transaction so role / tenant
/// state from prior calls cannot leak.
async fn fetch_titles_as_tenant(pool: &djogi::pg::pool::DjogiPool, tenant: &str) -> Vec<String> {
    let tenant_owned = tenant.to_owned();
    djogi::transaction::atomic(pool, |tx| {
        Box::pin(async move {
            tx.raw_execute("SET LOCAL ROLE djogi_rls_test_user", &[])
                .await?;
            tx.set_tenant(&tenant_owned).await?;
            let titles: Vec<String> = Document::objects()
                .fetch_all(tx)
                .await?
                .into_iter()
                .map(|d| d.title)
                .collect();
            Ok::<_, djogi::DjogiError>(titles)
        })
    })
    .await
    .unwrap_or_else(|e| panic!("fetch as tenant {tenant}: {e:?}"))
}

#[djogi::djogi_test]
async fn rls_with_fk_tenant_key_filters_correctly(mut ctx: djogi::DjogiContext) {
    setup(&mut ctx).await;

    // Seed owners + documents as the superuser test connection — RLS
    // does not apply here so we can lay down the fixture cleanly.
    // Owner ids are deterministic so the cross-tenant assertions can
    // compare against fixed values.
    ctx.raw_execute(
        "INSERT INTO owners (id, name) VALUES \
         (1000, 'org-a'), (2000, 'org-b')",
        &[],
    )
    .await
    .expect("seed owners");
    ctx.raw_execute(
        "INSERT INTO documents (owner_id, title) VALUES \
         (1000, 'org-a-doc'), (2000, 'org-b-doc')",
        &[],
    )
    .await
    .expect("seed documents");

    let pool = ctx.raw_pool().expect("test ctx must be pool-backed").clone();

    // Tenant A: only org-a-doc. Pre-fix #37 returned 0 rows because the
    // empty cast suffix produced an untyped current_setting() compared
    // to a BIGINT column, which Postgres treats as NULL after coercion
    // failure.
    assert_eq!(
        fetch_titles_as_tenant(&pool, "1000").await,
        vec!["org-a-doc".to_string()],
        "tenant 1000 must see exactly its own document",
    );

    // Tenant B: only org-b-doc. Switching tenants via set_tenant must
    // change the visible row set.
    assert_eq!(
        fetch_titles_as_tenant(&pool, "2000").await,
        vec!["org-b-doc".to_string()],
        "tenant 2000 must see exactly its own document",
    );

    // 9999 is a valid bigint that cleanly survives the `::bigint` cast
    // but matches no row, so the policy hides all documents. This
    // pinpoints "the cast worked AND filtering correctly excludes other
    // tenants" — pre-fix #37 the empty cast suffix would produce the
    // same zero-rows outcome for the wrong reason (NULL coercion),
    // making this a useful tightening of the assertions above.
    let nine_titles = fetch_titles_as_tenant(&pool, "9999").await;
    assert!(
        nine_titles.is_empty(),
        "tenant 9999 has no documents; policy must hide both org-a and org-b rows; \
         got {nine_titles:?}",
    );
}

/// The macro emits a side-channel `target/djogi_rls/{table}_rls.sql`
/// file containing the policy DDL it would apply once the
/// migration differ consumes RLS metadata. Pre-fix (#37) the file
/// contained `current_setting('app.tenant_id', true))` with an empty
/// suffix; post-fix it contains `current_setting('app.tenant_id',
/// true)::bigint)`. This test reads the file from the test crate's
/// `target/` (proc macros run with `CARGO_MANIFEST_DIR` set to the
/// crate being compiled) and asserts the literal `::bigint` is
/// present.
///
/// The file is written at macro-expansion time, so it exists from the
/// moment the test crate compiles — no runtime DDL emission required.
#[djogi::djogi_test]
async fn rls_policy_ddl_uses_bigint_cast(_ctx: djogi::DjogiContext) {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set");
    let manifest_dir =
        std::path::Path::new(&manifest_dir).canonicalize().expect("canonicalize CARGO_MANIFEST_DIR");
    let path = manifest_dir
        .join("target")
        .join("djogi_rls")
        .join("documents_rls.sql");
    assert!(
        path.starts_with(&manifest_dir),
        "RLS side-channel path should stay within manifest dir"
    );
    let contents = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read RLS side-channel at {}: {e}", path.display()));
    assert!(
        contents.contains("::bigint"),
        "RLS policy DDL must contain `::bigint` cast for ForeignKey<Owner> tenant_key \
         (HeerId inner PK is BIGINT). File contents:\n{contents}",
    );
    assert!(
        contents.contains("current_setting('app.tenant_id', true)::bigint"),
        "RLS policy DDL must apply the `::bigint` cast directly to current_setting(...) — \
         pre-fix #37 emitted an empty cast that compared a BIGINT column to an untyped \
         TEXT result. File contents:\n{contents}",
    );
}
