// Issue #462 — cross-model set operations: free constructors accept only
// QuerySet<M> arms. Passing a SetOpQuerySet must not compile.

use djogi::prelude::*;

#[model(table = "x462_cf_logins", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Login { pub actor: String }

#[model(table = "x462_cf_edits", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Edit { pub actor: String }

#[model(table = "x462_cf_activity", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Activity { pub actor: String }

fn main() {
    let already_combined = Login::objects().union(Login::objects());
    // MUST fail: SetOpQuerySet is not a QuerySet nor VisageQuerySet
    let _err = djogi::query::union_as::<Activity, _, _>(already_combined, Edit::objects());
}
