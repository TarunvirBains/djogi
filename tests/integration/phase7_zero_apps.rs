//! Phase 7-Zero T10 — two-app integration tests.
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

    #[app(database = "main", tombstone)]
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
fn registry_has_users_billing_subscription_oldbilling_plus_global() {
    let all = AppRegistry::all();
    let labels: Vec<&str> = all.iter().map(|d| d.label).collect();
    assert!(labels.contains(&""), "synthetic global bucket");
    assert!(labels.contains(&"users"));
    assert!(labels.contains(&"billing"));
    assert!(labels.contains(&"subscription"));
    assert!(labels.contains(&"oldbilling"));
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
    let edges = AppRegistry::cross_app_edges();
    // All T10 apps are in the `main` database, so every edge's
    // source_database and target_database are "main".
    for edge in edges {
        if edge.source_app == "billing" || edge.source_app == "subscription" {
            assert_eq!(edge.source_database, "main", "source_database");
            assert_eq!(edge.target_database, "main", "target_database");
        }
    }
}

#[test]
fn cross_app_cycles_empty_for_acyclic_graph() {
    // The T10 graph is billing→users, subscription→users — both
    // point at users, which has no outgoing edges. Acyclic.
    // (Other test integration files may introduce cycles; we only
    // assert ours doesn't, not that the global result is empty.)
    let cycles = AppRegistry::cross_app_cycles();
    let has_t10_cycle = cycles.iter().any(|c| {
        c.iter()
            .any(|id| id.label == "billing" || id.label == "subscription")
            && c.iter().any(|id| id.label == "users")
    });
    assert!(!has_t10_cycle, "unexpected T10 cycle: {cycles:?}");
}
