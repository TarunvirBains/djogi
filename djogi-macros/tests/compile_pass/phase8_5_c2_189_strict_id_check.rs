// Phase 8.5 Cluster 2 djogi#189 — `#[model(strict_ids)]` and
// `#[field(strict_id_check)]` opt-in attributes.
//
// Exercises the macro's parse + lower path:
//
// 1. Model-wide `#[model(strict_ids)]` on HeerId-PK model — applies
//    to the framework `id` column and to FK columns.
// 2. Model-wide `#[model(strict_ids)]` on RanjId-PK model — applies
//    to the framework `id` column on a UUID carrier.
// 3. Field-level `#[field(strict_id_check)]` on a bare HeerId user
//    column.
// 4. Field-level `#[field(strict_id_check)]` on a ForeignKey<T> field
//    where the model itself has no `#[model(strict_ids)]` (per-field
//    opt-in without bulk enabling).
// 5. `Option<HeerId>` with `#[field(strict_id_check)]` — nullable
//    HeerId-shaped column is acceptable (the validation strips one
//    `Option<…>` wrap).
// 6. Default-off (no opt-in attributes) on a HeerId-PK model — pre-189
//    backward compatibility.
//
// Compile-pass only — runtime / catalog behaviour is covered by
// `tests/internal/phase8_5_c2_189_strict_id_check.rs`.

use djogi::prelude::*;

// (1) Model-wide opt-in on HeerId PK.
#[model(table = "p189_strict_heer", pk = HeerId, strict_ids, no_default)]
#[derive(Debug, Clone)]
pub struct P189StrictHeer {
    pub label: String,
}

// (2) Model-wide opt-in on RanjId PK.
#[model(table = "p189_strict_ranj", pk = RanjId, strict_ids, no_default)]
#[derive(Debug, Clone)]
pub struct P189StrictRanj {
    pub label: String,
}

// (3) Field-level opt-in on a bare HeerId column (no model-wide flag).
#[model(table = "p189_field_optin", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct P189FieldOptin {
    #[field(strict_id_check)]
    pub external_owner: ::djogi::types::HeerId,
    pub label: String,
}

// (4) Field-level opt-in on an FK column (no model-wide flag).
#[model(table = "p189_fk_target", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct P189FkTarget {
    pub label: String,
}

#[model(table = "p189_fk_source", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct P189FkSource {
    #[field(strict_id_check)]
    pub owner_id: ForeignKey<P189FkTarget>,
    pub label: String,
}

// (5) Nullable bare HeerId with the opt-in attribute.
#[model(table = "p189_nullable_optin", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct P189NullableOptin {
    #[field(strict_id_check)]
    pub external_ref: ::std::option::Option<::djogi::types::HeerId>,
    pub label: ::std::option::Option<String>,
}

// (6) Default-off model — no opt-in, no strict CHECK.
#[model(table = "p189_default_off", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct P189DefaultOff {
    pub label: String,
}

fn main() {}
