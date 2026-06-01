// djogi#189 — `#[model(strict_ids)]` and
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
//    opt-in without bulk enabling) — HeerId-PK FK target.
// 5. `Option<HeerId>` with `#[field(strict_id_check)]` — nullable
//    HeerId-shaped column is acceptable (the validation strips one
//    `Option<…>` wrap).
// 6. Default-off (no opt-in attributes) on a HeerId-PK model — pre-189
//    backward compatibility.
// 7. Field-level `#[field(strict_id_check)]` on a ForeignKey<T> field
//    where the FK target has a Custom BIGINT PK — the macro must accept
//    the attribute (FK is a relation field, whitelisted) without emitting
//    a diagnostic. Runtime CHECK behaviour (none) is verified by the
//    integration suite.
// 8. Field-level `#[field(strict_id_check)]` on a ForeignKey<T> field
//    where the FK target has a Custom UUID PK — same as (7) but for a
//    UUID-carrier custom type.
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

// (7) Field-level opt-in on FK to Custom BIGINT target.
//
// The macro must accept `#[field(strict_id_check)]` on any FK column
// (relation fields are whitelisted). No compile-time error, no diagnostic.
// Whether a CHECK is actually emitted is decided at projection time (runtime)
// based on the FK target's PK family — tested in the integration suite.
djogi::primary_key! {
    pub struct P189CustomBigintId(i64);
    sql_type = "BIGINT";
    default_sql = "txid_current()";
}

#[model(table = "p189_custom_bigint_target", pk = P189CustomBigintId, no_default)]
#[derive(Debug, Clone)]
pub struct P189CustomBigintTarget {
    pub label: String,
}

#[model(table = "p189_field_fk_custom_bigint_src", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct P189FieldFkCustomBigint {
    #[field(strict_id_check)]
    pub owner_id: ForeignKey<P189CustomBigintTarget>,
    pub label: String,
}

// (8) Field-level opt-in on FK to Custom UUID target.
djogi::primary_key! {
    pub struct P189CustomUuidId(::uuid::Uuid);
    sql_type = "UUID";
    default_sql = "gen_random_uuid()";
}

#[model(table = "p189_custom_uuid_target", pk = P189CustomUuidId, no_default)]
#[derive(Debug, Clone)]
pub struct P189CustomUuidTarget {
    pub label: String,
}

#[model(table = "p189_field_fk_custom_uuid_src", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct P189FieldFkCustomUuid {
    #[field(strict_id_check)]
    pub owner_id: ForeignKey<P189CustomUuidTarget>,
    pub label: String,
}

fn main() {}
