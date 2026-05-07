// Phase 5-Zero T3 fixup: prefetch null-probe type safety for non-BIGINT PKs.
//
// Regression coverage for a Codex-flagged blocker in the T3 landing:
// `prefetch_loader` decoded the target's `id` column as `Option<i64>`
// to distinguish a LEFT JOIN miss from a real row. That probe works
// for the default `HeerId` (BIGINT) but silently fails for
// `pk = Serial` (INTEGER decode of `Option<i64>` errors) and
// `pk = RanjId` (UUID is not convertible to `i64`). With
// `.unwrap_or(true)`, the decode error was treated as "target absent"
// — dropping every joined target on non-BIGINT-PK models.
//
// The fixup decodes `Option<Target::Pk>` instead; this file exercises
// both the happy path (target present) and the "target absent"
// path (LEFT JOIN miss on a nullable FK) against a Serial-PK target
// to pin that the type-agnostic probe returns correct results on both
// branches and would regress loudly if a future refactor re-introduced
// the `Option<i64>` shape.
//
// Serial (`i32`) is chosen over RanjId for the fixture because it
// requires no additional DDL dependencies beyond the HeeRanjId schema
// the harness already installs and exercises the same class of bug
// (non-`i64` PK decode) with a smaller setup surface.

use djogi::prelude::*;

// ---------------------------------------------------------------------------
// Test models
// ---------------------------------------------------------------------------

// Serial-PK target: `pk = Serial` gives `id: i32`, so the prefetch
// null-probe must handle the non-BIGINT column type correctly. Before
// the fixup, the probe's `Option<i64>` decode failed and the
// `.unwrap_or(true)` path dropped every real row.
#[model(table = "t3_fixup_categories", pk = Serial)]
#[derive(Debug, Clone)]
pub struct Category {
    pub name: String,
}

// Source with an FK into the Serial-PK target. One non-null FK
// (`category_id`) for the happy-path test; one nullable FK
// (`secondary_category_id`) exercises the LEFT JOIN miss branch on
// a NULL source column, which is the other way a prefetch slot can
// end up `None`.
// Phase 7-Zero-2 T2 default flip — this model carries the HeerId-typed
// FK key back into the prefetch loader, so pin `pk = HeerId` explicitly.
#[model(table = "t3_fixup_items", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Item {
    pub label: String,
    pub category_id: ForeignKey<Category>,
    pub secondary_category_id: Option<ForeignKey<Category>>,
}

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

/// Build an `Item` with the required sentinel framework fields. `Item`
/// carries `no_default` because `ForeignKey<T>` intentionally does not
/// implement `Default` — same pattern as Phase 3's `vehicle_for_insert`.
fn item_for_insert(label: &str, category: &Category, secondary: Option<&Category>) -> Item {
    Item {
        id: <djogi::types::HeerId as djogi::PrimaryKey>::sentinel(),
        created_at: djogi::types::DateTime::UNIX_EPOCH,
        updated_at: djogi::types::DateTime::UNIX_EPOCH,
        label: label.into(),
        category_id: ForeignKey::new(category.id),
        secondary_category_id: secondary.map(|c| ForeignKey::new(c.id)),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Happy path: prefetch a Serial-PK target through a non-null FK.
/// Before the fixup, the `Option<i64>` null-probe erroneously classified
/// the INTEGER `id` column as absent (`unwrap_or(true)`), so
/// `row.get(ItemRelated::category())` returned `None` despite the join
/// matching a real row. The fixup makes the probe type-agnostic and
/// this path returns the resolved `&Category`.
#[djogi::djogi_test(sync_models = [Category, Item])]
async fn prefetch_resolves_serial_pk_target(mut ctx: djogi::DjogiContext) {
    let category = Category::create(
        &mut ctx,
        Category {
            name: "Books".into(),
            ..Default::default()
        },
    )
    .await
    .expect("category create");

    let _item = Item::create(&mut ctx, item_for_insert("A novel", &category, None))
        .await
        .expect("item create");

    let rows: Vec<PrefetchedRow<Item>> = Item::objects()
        .prefetch(ItemRelated::category())
        .fetch_all_prefetched(&mut ctx)
        .await
        .expect("fetch_all_prefetched should succeed against Serial-PK target");

    assert_eq!(rows.len(), 1, "one item seeded");
    let resolved = rows[0].get(ItemRelated::category()).expect(
        "Serial-PK target must resolve — this test is the regression pin \
             for the Option<i64> null-probe bug",
    );
    assert_eq!(resolved.name, "Books");
    assert_eq!(resolved.id, category.id);
}

/// LEFT JOIN miss path: nullable FK into a Serial-PK target. The source
/// column is NULL, so `category_id` (target) comes back NULL in the
/// joined row. The type-agnostic probe must surface that as `None` on
/// the prefetch slot — both before and after the fixup this branch
/// returned `None`, but the assertion pins the behaviour so a future
/// refactor (e.g. one that drops the null probe entirely in favour of
/// `from_pg_row`-on-every-row) cannot silently regress it.
#[djogi::djogi_test(sync_models = [Category, Item])]
async fn prefetch_nullable_fk_to_serial_pk_target_is_none(mut ctx: djogi::DjogiContext) {
    let primary = Category::create(
        &mut ctx,
        Category {
            name: "Primary".into(),
            ..Default::default()
        },
    )
    .await
    .expect("primary category create");

    let _item = Item::create(&mut ctx, item_for_insert("Solo", &primary, None))
        .await
        .expect("item create with null secondary");

    let rows: Vec<PrefetchedRow<Item>> = Item::objects()
        .prefetch(ItemRelated::secondary_category())
        .fetch_all_prefetched(&mut ctx)
        .await
        .expect("fetch_all_prefetched should succeed on nullable FK");

    assert_eq!(rows.len(), 1);
    assert!(
        rows[0].get(ItemRelated::secondary_category()).is_none(),
        "nullable FK with NULL source column must surface as None"
    );
}
