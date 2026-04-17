//! Query API — lazy `QuerySet<T>`, typed filters, SQL emission.
//!
//! The public surface is re-exported at crate root and in `prelude`:
//! users write `use djogi::prelude::*;` and get `QuerySet`, `FieldRef`,
//! `Lookup`, etc. without a second import.
//!
//! Internally: `queryset` holds the builder state, `condition` the filter
//! tree, `field` the typed column handles, `order` ordering expressions,
//! `filter` the programmatic-builder types, `update` bulk-update assignments,
//! `sql` the `ConditionBuilder` + SQL emitters, and `terminal` the `fetch_*`
//! methods. Splitting by responsibility keeps each file auditable.

pub mod condition;
pub mod field;

pub use condition::{Condition, FilterValue, Leaf, LookupOp};
pub use field::{FieldRef, IntoFilterValue};
