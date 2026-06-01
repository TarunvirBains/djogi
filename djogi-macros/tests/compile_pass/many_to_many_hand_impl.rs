// Verifies that a hand-written `impl ManyToMany<Target> for Source` with a
// typed `related()` / `add_related()` / `remove_related()` body compiles
// end-to-end against the trait shape (`&mut DjogiContext`
// in place of generic executor).
//
// Pinned invariants (all compile-time):
//
//   - `ManyToMany<Target>` is public and reachable via both
//     `djogi::relation::ManyToMany` and `djogi::prelude::*`.
//   - The trait has no default bodies for `related` / `add_related` /
//     `remove_related` — a user supplies them using the typed closure
//     filter API. The body here exercises that path so a future
//     regression that silently adds a default impl (or drops one of
//     the required methods) fails to compile this fixture.
//   - `Self: Model` flows through the trait's `Self` bound: `impl
//     ManyToMany<Group> for Person` only type-checks because
//     `#[derive(Model)]`-driven expansion emitted
//     `impl djogi::model::__sealed::Sealed for Person` alongside the
//     `Model` impl. A user cannot hand-impl `ManyToMany` on a type
//     that skipped `#[derive(Model)]`.
//   - `IntoFilterValue for ForeignKey<T>` is wired: the typed filter
//     body `f.person_id().eq(ForeignKey::new(self.id.clone()))`
//     compiles because the FK wrapper projects through its inner
//     `T::Pk`'s `IntoFilterValue` impl.
//   - The Task 7 `many_to_many!` macro stamps out this exact
//     shape on behalf of the user; this fixture locks in the hand-
//     written form that macro output must match.
//
// No live Postgres access here — the `&mut DjogiContext` receiver is
// never actually invoked at runtime; the `main` body only exercises
// compile-time trait probes.

use djogi::prelude::*;
use djogi::relation::{ForeignKey, ManyToMany};

#[model(table = "persons_mm")]
#[derive(Debug, Clone)]
pub struct Person {
    pub name: String,
}

#[model(table = "groups_mm")]
#[derive(Debug, Clone)]
pub struct Group {
    pub name: String,
}

// `through` marks this as a junction model (is_through = true on the
// descriptor). `no_default` because `ForeignKey<T>` deliberately has no
// `Default` impl — a relation with no PK value is meaningless.
#[model(table = "person_groups_mm", through, no_default)]
#[derive(Debug, Clone)]
pub struct PersonGroup {
    pub person_id: ForeignKey<Person>,
    pub group_id: ForeignKey<Group>,
    pub role: String,
}

// Hand-written direction: "a Person has many Groups via PersonGroup".
// The reverse direction ("a Group has many Persons") would be a
// symmetric impl keyed on `ManyToMany<Person> for Group`.
impl ManyToMany<Group> for Person {
    type Through = PersonGroup;
    const RELATION: &'static str = "groups";

    fn this_fk() -> &'static str {
        "person_id"
    }
    fn that_fk() -> &'static str {
        "group_id"
    }

    async fn related<'ctx>(
        &'ctx self,
        ctx: &'ctx mut DjogiContext,
    ) -> Result<Vec<Group>, DjogiError> {
        // Step 1: fetch junction rows pointing at `self`. The typed
        // closure filter compares the `person_id` column (a
        // `FieldRef<PersonGroup, ForeignKey<Person>>`) to a freshly-built
        // `ForeignKey<Person>` constructed from `self.pk_value()`; the
        // `IntoFilterValue` blanket on `ForeignKey<T>` projects that
        // through `T::Pk` to the matching `FilterValue` variant.
        //
        // Reborrow `ctx` (`&mut *ctx`) into the terminal so the outer
        // `ctx` binding remains usable for Step 2.
        let through_rows: Vec<PersonGroup> = PersonGroup::objects()
            .filter(|f| f.person_id().eq(ForeignKey::new(self.id.clone())))
            .fetch_all(&mut *ctx)
            .await?;

        // Step 2: project the junction rows down to target PKs. The
        // `many_to_many!` macro emits an `IN (…)` query against `Target`;
        // pending a typed `.r#in(...)` lookup we fetch each target by
        // PK through `Group::get`. Two queries → N+1 is acceptable for
        // the hand-written reference impl; the macro form is free to
        // fold this into a single `WHERE id IN (...)` SELECT.
        let mut out: Vec<Group> = Vec::with_capacity(through_rows.len());
        for row in &through_rows {
            let group = Group::get(&mut *ctx, row.group_id.key()).await?;
            out.push(group);
        }
        Ok(out)
    }

    async fn add_related<'ctx>(
        &'ctx self,
        ctx: &'ctx mut DjogiContext,
        target: &'ctx Group,
        extras: PersonGroup,
    ) -> Result<PersonGroup, DjogiError> {
        // Overwrite the junction's FK columns with freshly-built
        // references to self / target; the caller-supplied `extras`
        // keeps its `role` (and any other non-FK junction columns).
        let junction = PersonGroup {
            person_id: ForeignKey::new(self.id.clone()),
            group_id: ForeignKey::new(target.id.clone()),
            ..extras
        };
        PersonGroup::create(ctx, junction).await
    }

    async fn remove_related<'ctx>(
        &'ctx self,
        ctx: &'ctx mut DjogiContext,
        target: &'ctx Group,
    ) -> Result<u64, DjogiError> {
        // The canonical delete body — composing two typed filters (by
        // `person_id` and `group_id`) on the through queryset's AND
        // tree and deferring to its bulk `.delete(ctx)` terminal.
        PersonGroup::objects()
            .filter(|f| f.person_id().eq(ForeignKey::new(self.id.clone())))
            .filter(|f| f.group_id().eq(ForeignKey::new(target.id.clone())))
            .delete(ctx)
            .await
    }
}

// Compile-only probes — each asserts a specific trait-shape invariant.

fn _relation_const_and_fk_fns_exist() {
    // `RELATION` is an associated `const &'static str`; `this_fk` and
    // `that_fk` are `fn() -> &'static str` with no `self` receiver.
    let _name: &'static str = <Person as ManyToMany<Group>>::RELATION;
    let _this: &'static str = <Person as ManyToMany<Group>>::this_fk();
    let _that: &'static str = <Person as ManyToMany<Group>>::that_fk();
}

fn _through_associated_type_is_model() {
    // `type Through: Model` — compile-only check: we can name the
    // through type as a `Model` and call `Model::table_name()` on it.
    fn _is_model<M: Model>() -> &'static str {
        M::table_name()
    }
    let _t: &'static str = _is_model::<<Person as ManyToMany<Group>>::Through>();
}

fn main() {
    // Runtime sanity — pin the concrete string literals so any
    // accidental swap between `this_fk` / `that_fk` in the impl body
    // fails loudly rather than silently producing `WHERE
    // group_id = <person id>` SQL in the generated shape.
    assert_eq!(<Person as ManyToMany<Group>>::RELATION, "groups");
    assert_eq!(<Person as ManyToMany<Group>>::this_fk(), "person_id");
    assert_eq!(<Person as ManyToMany<Group>>::that_fk(), "group_id");

    // The `Through` associated type resolves to a full Model with
    // its own `table_name()` — confirms the junction stays queryable
    // via `PersonGroup::objects()`.
    assert_eq!(
        <<Person as ManyToMany<Group>>::Through as Model>::table_name(),
        "person_groups_mm"
    );

    // `is_through = true` on the junction, `false` on the endpoints —
    // redundant with the `through_model.rs` fixture but pinned here
    // too so a future refactor that accidentally decouples
    // `ManyToMany` from the `through` flag fails this fixture.
    assert!(<<Person as ManyToMany<Group>>::Through as Model>::descriptor().is_through);
    assert!(!<Person as Model>::descriptor().is_through);
    assert!(!<Group as Model>::descriptor().is_through);
}
