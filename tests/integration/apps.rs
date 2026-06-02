//! T10 — two-app integration tests.
//!
//! Pure descriptor tests: exercise the apps subsystem end-to-end
//! without a database. Verifies:
//!
//! 1. Two apps + models declared in one crate → both appear in
//!    `AppRegistry::all()` with correct labels and database targets.
//! 2. `#[model(app = X)]` lowers to `ModelDescriptor.app =
//!    Some(<X as App>::LABEL)` at const-eval time.
//! 3. Cross-app FK: `Invoice (app = Billing)` → `User (app = Users)`
//!    surfaces in `AppRegistry::cross_app_edges()`.
//! 4. Intra-app FK does NOT appear in cross_app_edges.
//! 5. `renamed_from` round-trips through `AppDescriptor`.
//! 6. `tombstone` round-trips through `AppDescriptor` and
//!    `App::TOMBSTONE`.
//! 7. `moved_from_app` round-trips through `ModelDescriptor`.

use djogi::prelude::*;

djogi::apps! {
    #[app(database = "main")]
    pub struct Users;

    #[app(database = "main")]
    pub struct Billing;

    #[app(database = "main", renamed_from = "subscription_old")]
    pub struct Subscription;

    // OldBilling tombstoned on a *different* database to prove that
    // database-field propagation through AppDescriptor + the
    // cross-app graph actually uses the declared target (not a
    // fallback default).
    #[app(database = "crud_log", tombstone)]
    pub struct OldBilling;
}

#[model(table = "t10_users", app = Users)]
#[derive(Debug, Clone)]
pub struct User {
    pub email: String,
}

#[model(table = "t10_invoices", app = Billing, no_default)]
#[derive(Debug, Clone)]
pub struct Invoice {
    pub customer: ForeignKey<User>,
    pub amount_cents: i64,
}

// Intra-app FK: LineItem → Invoice, both in Billing. Must NOT
// appear in cross_app_edges.
#[model(table = "t10_line_items", app = Billing, no_default)]
#[derive(Debug, Clone)]
pub struct LineItem {
    pub invoice: ForeignKey<Invoice>,
    pub description: String,
}

// Move scenario: active model in Billing, historical reference to
// the tombstoned OldBilling via moved_from_app. This is legal
// (tombstoned apps are valid `moved_from_app` targets).
#[model(table = "t10_subscriptions", app = Subscription, moved_from_app = OldBilling, no_default)]
#[derive(Debug, Clone)]
pub struct Sub {
    pub subscriber: ForeignKey<User>,
}

#[test]
fn registry_has_exact_set_of_apps() {
    // Each integration-test binary is its own crate, so inventory
    // is isolated — we can assert the exact (label, database) pairs
    // in registry order. Sort key is label-first (empty bucket
    // sorts first), then database as tiebreaker.
    let all = AppRegistry::all();
    let shape: Vec<(&str, &str)> = all.iter().map(|d| (d.label, d.database)).collect();
    assert_eq!(
        shape,
        vec![
            ("", "main"), // synthetic global
            ("billing", "main"),
            ("oldbilling", "crud_log"), // different database than the rest
            ("subscription", "main"),
            ("users", "main"),
        ]
    );
}

#[test]
fn renamed_from_roundtrips() {
    let sub = AppRegistry::all()
        .iter()
        .find(|d| d.label == "subscription")
        .expect("Subscription app registered");
    assert_eq!(sub.renamed_from, Some("subscription_old"));
    assert!(!sub.tombstone);
}

#[test]
fn tombstone_roundtrips() {
    let old_billing = AppRegistry::all()
        .iter()
        .find(|d| d.label == "oldbilling")
        .expect("OldBilling app registered");
    assert!(old_billing.tombstone);
    assert_eq!(old_billing.renamed_from, None);
    // Trait const matches the descriptor. `const { assert!(…) }`
    // keeps the check compile-time so clippy stops complaining
    // about asserting a constant at runtime.
    const _: () = assert!(<OldBilling as App>::TOMBSTONE);
}

#[test]
fn model_app_label_is_const_eval_of_app_type() {
    let user_desc = User::descriptor();
    assert_eq!(user_desc.app, Some("users"));
    assert_eq!(user_desc.moved_from_app, None);

    let invoice_desc = Invoice::descriptor();
    assert_eq!(invoice_desc.app, Some("billing"));

    let sub_desc = Sub::descriptor();
    assert_eq!(sub_desc.app, Some("subscription"));
    assert_eq!(sub_desc.moved_from_app, Some("oldbilling"));
}

#[test]
fn cross_app_edge_from_invoice_to_user_appears() {
    let edges = AppRegistry::cross_app_edges();
    // Invoice.customer: ForeignKey<User>, billing → users.
    let found = edges.iter().any(|e| {
        e.source_app == "billing"
            && e.source_type == "Invoice"
            && e.target_app == "users"
            && e.target_type == "User"
    });
    assert!(found, "expected billing→users edge in {edges:?}");
}

#[test]
fn intra_app_fk_does_not_appear_in_cross_app_edges() {
    let edges = AppRegistry::cross_app_edges();
    // LineItem.invoice is billing → billing; must NOT be reported.
    let intra = edges
        .iter()
        .any(|e| e.source_type == "LineItem" && e.target_type == "Invoice");
    assert!(
        !intra,
        "intra-app FK LineItem→Invoice should not appear in cross_app_edges"
    );
}

#[test]
fn sub_to_user_is_cross_app() {
    // Sub.user: ForeignKey<User>. Sub is in Subscription, User is
    // in Users — another cross-app edge.
    let edges = AppRegistry::cross_app_edges();
    let found = edges.iter().any(|e| {
        e.source_app == "subscription"
            && e.source_type == "Sub"
            && e.target_app == "users"
            && e.target_type == "User"
    });
    assert!(found, "expected subscription→users edge in {edges:?}");
}

#[test]
fn cross_app_edges_carry_database_fields() {
    // OldBilling lives in `crud_log`, every other T10 app lives in
    // `main`. The graph has no FK into OldBilling (tombstoned, no
    // active models), so every cross-app edge is within `main`:
    let edges = AppRegistry::cross_app_edges();
    assert!(!edges.is_empty(), "T10 declares cross-app FKs");
    for edge in edges {
        assert_eq!(edge.source_database, "main", "{edge:?}");
        assert_eq!(edge.target_database, "main", "{edge:?}");
    }
    // Sanity-check the lookup can *produce* non-main databases too,
    // via the registry directly — proves the database field isn't
    // a default fallback:
    let old_billing = AppRegistry::all()
        .iter()
        .find(|d| d.label == "oldbilling")
        .expect("OldBilling registered");
    assert_eq!(old_billing.database, "crud_log");
}

#[test]
fn cross_app_cycles_empty_for_acyclic_graph() {
    // Each integration-test binary has isolated inventory, so we
    // can assert the *global* cycle result is empty — the T10
    // graph is billing→users, subscription→users, both pointing at
    // Users which has no outgoing edges. Acyclic.
    let cycles = AppRegistry::cross_app_cycles();
    assert!(cycles.is_empty(), "expected zero cycles; got {cycles:?}");
}
