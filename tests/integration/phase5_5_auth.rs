//! Phase 5.5 integration tests — auth substrate.
//!
//! Task 1 scope (this file, initially): `DjogiContext::with_auth` attaches
//! an `AuthContext` that can be read back via `ctx.auth()`.
//!
//! Later Phase 5.5 tasks extend this file (Task 4 password_hash_round_trips,
//! Task 10 auto_set_tenant_from_auth, Task 11 with_auth_insecurely_emits_warn).

use djogi::auth::AuthContext;
use djogi::prelude::*;

#[djogi::djogi_test]
async fn with_auth_attaches_and_reads_back(mut ctx: djogi::DjogiContext) {
    let auth = AuthContext::new(HeerId::from_i64(42).unwrap())
        .with_tenant("org_a")
        .with_scopes(vec!["read".into(), "write".into()]);
    let ctx = ctx.with_auth(auth.clone());
    let attached = ctx.auth().expect("auth attached");
    assert_eq!(attached.user_id, auth.user_id);
    assert_eq!(attached.tenant_id, Some("org_a".into()));
    assert!(attached.has_scope("read"));
}
