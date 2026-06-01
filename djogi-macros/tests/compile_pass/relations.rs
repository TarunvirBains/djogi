// Verifies that Task 2's relation emission compiles end-to-end:
//
//   - `#[model]` structs carrying `ForeignKey<T>`, `Option<ForeignKey<T>>`,
//     and `OneToOneField<T>` type-check without adjustments;
//   - the macro-emitted `{Model}Related` accessor struct exposes one method
//     per relation field, each returning the expected
//     `RelationPath<Source, Target>` type;
//   - the method-name convention (`owner_id` → `owner()`, `fuel_type_id` →
//     `fuel_type()`) produces callable paths;
//   - `RelationKind::ForeignKey` / `RelationKind::OneToOne` round-trip
//     through the emitted paths and can be asserted on;
//   - `#[field(on_delete = "cascade")]` is accepted on an FK field (the
//     value is propagated to `FieldDescriptor::on_delete` — runtime check
//     lands in the Task 3 integration tests).
//
// This is the compile-pass counterpart to `basic_inject.rs` /
// `fields_accessor.rs` — it pins the core Task 2 acceptance and falls back
// on the `_raw_ident_column_literal_strips_prefix`-style runtime assertion
// pattern to guard against accidental regressions in the emitted literals
// (e.g. if a future change forgot to strip `_id` or route the kind through
// the right enum variant).

use djogi::prelude::*;
use djogi::relation::{ForeignKey, OneToOneField, RelationKind, RelationPath};

#[model(table = "owners")]
#[derive(Debug, Clone)]
pub struct Owner {
    pub name: String,
}

#[model(table = "fuel_types")]
#[derive(Debug, Clone)]
pub struct FuelType {
    pub name: String,
}

// `ForeignKey<T>` deliberately does not implement `Default` — a relation
// without a PK value is meaningless. `no_default` is the corresponding
// opt-out on the model side (see `ModelAttrs::no_default`), letting the
// struct carry non-Default field types without trying to derive a Default
// impl that would reference them.
#[model(table = "vehicles", no_default)]
#[derive(Debug, Clone)]
pub struct Vehicle {
    pub make: String,
    // Non-null FK with an explicit on_delete — exercises the attr-parse
    // path all the way through descriptor emission.
    #[field(on_delete = "cascade")]
    pub owner_id: ForeignKey<Owner>,
    // Nullable FK — exercises the `Option<ForeignKey<T>>` branch in
    // `detect_relation`.
    pub fuel_type_id: Option<ForeignKey<FuelType>>,
}

#[model(table = "users")]
#[derive(Debug, Clone)]
pub struct User {
    pub email: String,
}

#[model(table = "profiles", no_default)]
#[derive(Debug, Clone)]
pub struct Profile {
    pub bio: String,
    pub user_id: OneToOneField<User>,
    // Nullable O2O — exercises the `Option<OneToOneField<T>>` branch in
    // `detect_relation`, rounding out the four advertised relation shapes
    // (FK, Option<FK>, O2O, Option<O2O>). Without this the O2O-with-Option
    // permutation was unpinned.
    pub optional_profile: Option<OneToOneField<User>>,
}

// Fully-qualified target path — the target type is defined in an inner
// module and referenced via `inner::Widget` inside the relation wrapper.
// This pins the path-preservation fix: `detect_relation` / `relations::expand`
// must emit the full `inner::Widget` in the generated `RelationPath<…, _>`
// return type, not just the last-segment ident `Widget` (which would fail
// to resolve here because `Widget` is not imported into the outer scope).
mod inner {
    use djogi::prelude::*;

    #[model(table = "widgets")]
    #[derive(Debug, Clone)]
    pub struct Widget {
        pub kind: String,
    }
}

#[model(table = "orders", no_default)]
#[derive(Debug, Clone)]
pub struct Order {
    pub note: String,
    // FK target spelled with a module prefix — verifies the emitter
    // preserves `inner::Widget` end-to-end. If the emitter collapsed to
    // just `Widget` the compile would fail with `cannot find type Widget`.
    pub widget_id: ForeignKey<inner::Widget>,
}

fn _related_methods_return_typed_paths() {
    // Primary compile-pass acceptance: each method returns the specifically
    // typed `RelationPath<Source, Target>`. A mismatch on either generic
    // fails at type-check.
    let _p1: RelationPath<Vehicle, Owner> = VehicleRelated::owner();
    let _p2: RelationPath<Vehicle, FuelType> = VehicleRelated::fuel_type();
    let _p3: RelationPath<Profile, User> = ProfileRelated::user();
    let _p4: RelationPath<Profile, User> = ProfileRelated::optional_profile();
    // Fully-qualified target path preserved verbatim in the emitted
    // `RelationPath<_, inner::Widget>` return type.
    let _p5: RelationPath<Order, inner::Widget> = OrderRelated::widget();
}

fn _relation_metadata_accessors_compile() {
    // Each `RelationPath` exposes `source_column()`, `target_table()`, and
    // `kind()` — these are the three accessors Tasks 4 + 5 consume.
    let p = VehicleRelated::owner();
    let _: &'static str = p.source_column();
    let _: &'static str = p.target_table();
    let _: RelationKind = p.kind();
}

fn main() {
    // Runtime assertions — the compile-pass fixture is executed as a plain
    // binary under lihaaf, so we can pin the concrete values here and
    // catch accidental emitter drift (wrong column literal, wrong kind,
    // wrong target table) loudly rather than silently producing bogus SQL.
    let p_owner = VehicleRelated::owner();
    assert_eq!(p_owner.source_column(), "owner_id");
    assert_eq!(p_owner.target_table(), "owners");
    assert_eq!(p_owner.kind(), RelationKind::ForeignKey);

    let p_fuel = VehicleRelated::fuel_type();
    assert_eq!(p_fuel.source_column(), "fuel_type_id");
    assert_eq!(p_fuel.target_table(), "fuel_types");
    // Nullable FK still classifies as ForeignKey — the nullability lives on
    // the column, not on the relation kind.
    assert_eq!(p_fuel.kind(), RelationKind::ForeignKey);

    let p_user = ProfileRelated::user();
    assert_eq!(p_user.source_column(), "user_id");
    assert_eq!(p_user.target_table(), "users");
    assert_eq!(p_user.kind(), RelationKind::OneToOne);

    // `Option<OneToOneField<User>>` — nullable O2O still classifies as
    // OneToOne; the column name keeps its full identifier (no `_id`
    // suffix to strip), so the method stem matches the field name.
    let p_opt_profile = ProfileRelated::optional_profile();
    assert_eq!(p_opt_profile.source_column(), "optional_profile");
    assert_eq!(p_opt_profile.target_table(), "users");
    assert_eq!(p_opt_profile.kind(), RelationKind::OneToOne);

    // Fully-qualified target path — `ForeignKey<inner::Widget>`. The
    // emitted method returns `RelationPath<Order, inner::Widget>`, which
    // only resolves because `relations::expand` preserves the full target
    // type (`inner::Widget`) in the return-type position rather than
    // collapsing to the last-segment ident (`Widget`, which is not in
    // scope here).
    let p_widget = OrderRelated::widget();
    assert_eq!(p_widget.source_column(), "widget_id");
    assert_eq!(p_widget.target_table(), "widgets");
    assert_eq!(p_widget.kind(), RelationKind::ForeignKey);
}
