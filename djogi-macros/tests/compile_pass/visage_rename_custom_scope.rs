//! `visage_names(...)` can rename a custom-scope visage declared via
//! `visage_scopes(...)`, not just the four built-in scopes.
use djogi::prelude::*;

#[model(
    table = "visage_rename_custom_scope_users",
    visage_scopes(support = Support),
    visage_names(support = SupportTicketView)
)]
#[derive(Debug, Clone)]
pub struct User {
    #[field(expose(public, support))]
    pub email: String,
}

fn main() {
    let u = User::default();
    // Custom name resolves.
    let _v: SupportTicketView = SupportTicketView::from(&u);
    // Canonical custom-scope name still aliases to it.
    let _alias: UserSupport = SupportTicketView::from(&u);
}
