// Phase 8.5 Cluster 4B (djogi#106) — typed INSERT...SELECT integration
// tests against a live Postgres.
//
// Pins the live-DB behaviour of the new
// `QuerySet::insert_into::<T>(|t, s| ...)` surface introduced in this
// branch. Unit-level SQL-text shape coverage lives in
// `djogi/src/query/sql.rs` and `djogi/src/query/insert_select.rs`; this
// file proves the row-copy round trip works end-to-end against a real
// database — projection alignment, framework-column defaults, WHERE
// composition, the `none()` short-circuit, validation rejections, and
// ORDER-BY+LIMIT chunking.
//
// Two models — `Phase85C4bSource` and `Phase85C4bArchive` — share
// shape so the column mapping is unambiguous. The archive carries an
// `original_id` column so the source's id is copied without colliding
// with the framework `id` slot on the target.

use djogi::prelude::*;

#[model(table = "phase8_5_c4b_sources", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct Phase85C4bSource {
    pub label: String,
    pub published: bool,
    pub view_count: i32,
}

#[model(table = "phase8_5_c4b_archives", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct Phase85C4bArchive {
    /// Source row's id, preserved as a user column rather than the
    /// framework `id` slot on the archive (the archive's `id` is
    /// generated fresh per archive row, matching `Model::create`'s
    /// framework-column semantics). The column type matches the
    /// source's PK type (`HeerIdDesc`) so the closure-built column
    /// mapping type-checks at compile time —
    /// `target.original_id().copy_from(source.id().as_insert_source())`
    /// where both sides resolve to `HeerIdDesc`. This is the Phase
    /// 7-Zero-2 "ambient PK kinds are usable in any field position"
    /// pattern.
    pub original_id: djogi::HeerIdDesc,
    pub label: String,
    pub published: bool,
    pub view_count: i32,
}

async fn seed_sources(ctx: &mut djogi::DjogiContext) -> Vec<Phase85C4bSource> {
    let mut out = Vec::new();
    for (label, published, view_count) in [
        ("alpha", true, 100i32),
        ("beta", true, 50),
        ("gamma", false, 200),
        ("delta", true, 25),
    ] {
        let row = Phase85C4bSource::create(
            ctx,
            Phase85C4bSource {
                label: label.to_string(),
                published,
                view_count,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        out.push(row);
    }
    out
}

// ── Happy path ────────────────────────────────────────────────────────────

#[djogi::djogi_test(sync_models = [Phase85C4bSource, Phase85C4bArchive])]
async fn insert_select_copies_all_rows_no_filter(mut ctx: djogi::DjogiContext) {
    // No WHERE filter on the source — every row in the source table
    // lands in the archive. Affected-row count matches the source
    // count, and the archive's framework columns (`id`, `created_at`,
    // `updated_at`) come from column defaults.
    let _seeded = seed_sources(&mut ctx).await;

    let n = Phase85C4bSource::objects()
        .insert_into::<Phase85C4bArchive, _, _>(|t, s| {
            vec![
                t.original_id().copy_from(s.id().as_insert_source()),
                t.label().copy_from(s.label().as_insert_source()),
                t.published().copy_from(s.published().as_insert_source()),
                t.view_count().copy_from(s.view_count().as_insert_source()),
            ]
        })
        .execute(&mut ctx)
        .await
        .unwrap();
    assert_eq!(n, 4, "expected 4 source rows to be copied into the archive");

    let archived = Phase85C4bArchive::objects().count(&mut ctx).await.unwrap();
    assert_eq!(archived, 4);
}

#[djogi::djogi_test(sync_models = [Phase85C4bSource, Phase85C4bArchive])]
async fn insert_select_with_filter_copies_subset(mut ctx: djogi::DjogiContext) {
    // WHERE clause narrows the source set. Only published rows land
    // in the archive — three of the four seeded rows.
    let _seeded = seed_sources(&mut ctx).await;

    let n = Phase85C4bSource::objects()
        .filter(|f| f.published().eq(true))
        .insert_into::<Phase85C4bArchive, _, _>(|t, s| {
            vec![
                t.original_id().copy_from(s.id().as_insert_source()),
                t.label().copy_from(s.label().as_insert_source()),
                t.published().copy_from(s.published().as_insert_source()),
                t.view_count().copy_from(s.view_count().as_insert_source()),
            ]
        })
        .execute(&mut ctx)
        .await
        .unwrap();
    assert_eq!(n, 3);

    let archived_published = Phase85C4bArchive::objects()
        .filter(|f| f.published().eq(true))
        .count(&mut ctx)
        .await
        .unwrap();
    assert_eq!(archived_published, 3);

    let archived_unpublished = Phase85C4bArchive::objects()
        .filter(|f| f.published().eq(false))
        .count(&mut ctx)
        .await
        .unwrap();
    assert_eq!(
        archived_unpublished, 0,
        "filter on the source must exclude unpublished rows from the archive"
    );
}

#[djogi::djogi_test(sync_models = [Phase85C4bSource, Phase85C4bArchive])]
async fn insert_select_with_literal_source_emits_constant(mut ctx: djogi::DjogiContext) {
    // Source operand is `InsertSelectSource::literal(...)` — every
    // archived row gets the same constant value for that column.
    // Useful for archival markers ("status = ARCHIVED for every row").
    let _seeded = seed_sources(&mut ctx).await;

    let n = Phase85C4bSource::objects()
        .insert_into::<Phase85C4bArchive, _, _>(|t, s| {
            vec![
                t.original_id().copy_from(s.id().as_insert_source()),
                t.label().copy_from(s.label().as_insert_source()),
                t.published().copy_from(s.published().as_insert_source()),
                // Constant — every archived row gets view_count = 0.
                // `InsertSelectSource::literal` is polymorphic in the
                // source model; `S` is inferred from the closure's
                // return type as the enclosing source model
                // (`Phase85C4bSource`).
                t.view_count().copy_from(InsertSelectSource::literal(0i32)),
            ]
        })
        .execute(&mut ctx)
        .await
        .unwrap();
    assert_eq!(n, 4);

    let all_zero = Phase85C4bArchive::objects()
        .filter(|f| f.view_count().eq(0i32))
        .count(&mut ctx)
        .await
        .unwrap();
    assert_eq!(all_zero, 4, "literal source must apply to every row");
}

#[djogi::djogi_test(sync_models = [Phase85C4bSource, Phase85C4bArchive])]
async fn insert_select_framework_columns_populated_by_defaults(mut ctx: djogi::DjogiContext) {
    // Pin the framework-column contract: archive rows get fresh `id`,
    // `created_at`, `updated_at` from the target's column defaults —
    // they are NOT copied from the source's framework columns.
    let seeded = seed_sources(&mut ctx).await;
    // `HeerIdRecencyBiased` decodes as `HeerIdDesc`; `.as_i64()` is the
    // canonical accessor for the underlying stored bits — see
    // heeranjid/src/heer_desc.rs.
    let source_ids: Vec<i64> = seeded.iter().map(|r| r.id.as_i64()).collect();

    let _n = Phase85C4bSource::objects()
        .insert_into::<Phase85C4bArchive, _, _>(|t, s| {
            vec![
                t.original_id().copy_from(s.id().as_insert_source()),
                t.label().copy_from(s.label().as_insert_source()),
                t.published().copy_from(s.published().as_insert_source()),
                t.view_count().copy_from(s.view_count().as_insert_source()),
            ]
        })
        .execute(&mut ctx)
        .await
        .unwrap();

    // Archive ids are FRESH (not the source ids). Convert HeerIdDesc
    // values into the raw i64 backing them for the comparison.
    let archives = Phase85C4bArchive::objects()
        .fetch_all(&mut ctx)
        .await
        .unwrap();
    assert_eq!(archives.len(), 4);
    for archive in &archives {
        let archive_id: i64 = archive.id.as_i64();
        assert!(
            !source_ids.contains(&archive_id),
            "archive id {} collided with a source id — framework columns must \
             come from the target's defaults, not the source's projection",
            archive_id,
        );
    }

    // The `original_id` user column DOES carry the source id — that
    // is the explicit mapping the closure declared. The column is
    // typed `HeerIdDesc`, so unwrap to `i64` for the comparison
    // against the seeded source ids.
    let mut original_ids: Vec<i64> = archives.iter().map(|a| a.original_id.as_i64()).collect();
    original_ids.sort();
    let mut expected_source_ids = source_ids.clone();
    expected_source_ids.sort();
    assert_eq!(original_ids, expected_source_ids);
}

#[djogi::djogi_test(sync_models = [Phase85C4bSource, Phase85C4bArchive])]
async fn insert_select_with_order_by_and_limit_chunks_oldest_first(mut ctx: djogi::DjogiContext) {
    // ORDER BY + LIMIT compose with INSERT...SELECT — pick the two
    // rows with the LOWEST `view_count` and copy only them. Useful for
    // chunked archival ("archive 100 rows at a time, oldest first").
    let _seeded = seed_sources(&mut ctx).await;

    let n = Phase85C4bSource::objects()
        .order_by(|f| f.view_count().asc())
        .limit(2)
        .insert_into::<Phase85C4bArchive, _, _>(|t, s| {
            vec![
                t.original_id().copy_from(s.id().as_insert_source()),
                t.label().copy_from(s.label().as_insert_source()),
                t.published().copy_from(s.published().as_insert_source()),
                t.view_count().copy_from(s.view_count().as_insert_source()),
            ]
        })
        .execute(&mut ctx)
        .await
        .unwrap();
    assert_eq!(n, 2);

    // The two archived rows are the ones with the lowest view_count
    // (delta=25, beta=50).
    let archive_view_counts: std::collections::HashSet<i32> = Phase85C4bArchive::objects()
        .fetch_all(&mut ctx)
        .await
        .unwrap()
        .iter()
        .map(|a| a.view_count)
        .collect();
    assert_eq!(
        archive_view_counts,
        [25i32, 50i32].into_iter().collect(),
        "ORDER BY view_count ASC LIMIT 2 must pick the two lowest-view-count rows"
    );
}

// ── Short-circuit ──────────────────────────────────────────────────────────

#[djogi::djogi_test(sync_models = [Phase85C4bSource, Phase85C4bArchive])]
async fn insert_select_none_short_circuits(mut ctx: djogi::DjogiContext) {
    // Source is `QuerySet::none()`-derived — terminal returns 0 without
    // touching the database. Pin the empty-contract behaviour matches
    // bulk update / bulk delete.
    let _seeded = seed_sources(&mut ctx).await;

    let n = Phase85C4bSource::objects()
        .none()
        .insert_into::<Phase85C4bArchive, _, _>(|t, s| {
            vec![
                t.original_id().copy_from(s.id().as_insert_source()),
                t.label().copy_from(s.label().as_insert_source()),
                t.published().copy_from(s.published().as_insert_source()),
                t.view_count().copy_from(s.view_count().as_insert_source()),
            ]
        })
        .execute(&mut ctx)
        .await
        .unwrap();
    assert_eq!(n, 0);

    let archive_count = Phase85C4bArchive::objects().count(&mut ctx).await.unwrap();
    assert_eq!(
        archive_count, 0,
        "none().insert_into() must not touch any row"
    );

    // Source table is unchanged.
    let source_count = Phase85C4bSource::objects().count(&mut ctx).await.unwrap();
    assert_eq!(source_count, 4);
}

// ── Validation rejections ──────────────────────────────────────────────────

#[djogi::djogi_test(sync_models = [Phase85C4bSource, Phase85C4bArchive])]
async fn insert_select_rejects_empty_column_mapping(mut ctx: djogi::DjogiContext) {
    // Empty Vec — INSERT INTO target () SELECT FROM source is invalid
    // SQL. The terminal pre-validates and returns DjogiError::Validation
    // before the SQL leaves the framework.
    let _seeded = seed_sources(&mut ctx).await;

    let err = Phase85C4bSource::objects()
        .insert_into::<Phase85C4bArchive, _, _>(|_t, _s| {
            Vec::<InsertSelectColumn<Phase85C4bSource, Phase85C4bArchive>>::new()
        })
        .execute(&mut ctx)
        .await
        .unwrap_err();
    assert!(
        matches!(err, DjogiError::Validation(ref msg) if msg.contains("column mapping is empty")),
        "expected Validation error for empty column mapping, got: {err:?}",
    );

    // No row landed in the archive — pre-flight rejection means the
    // SQL never ran.
    let archive_count = Phase85C4bArchive::objects().count(&mut ctx).await.unwrap();
    assert_eq!(archive_count, 0);
}

#[djogi::djogi_test(sync_models = [Phase85C4bSource, Phase85C4bArchive])]
async fn insert_select_rejects_duplicate_target_columns(mut ctx: djogi::DjogiContext) {
    // Map two different source expressions to the same target column.
    // Postgres would reject with SQLSTATE 42701; djogi pre-validates so
    // the diagnostic carries the target column name.
    let _seeded = seed_sources(&mut ctx).await;

    let err = Phase85C4bSource::objects()
        .insert_into::<Phase85C4bArchive, _, _>(|t, s| {
            vec![
                t.label().copy_from(s.label().as_insert_source()),
                // Duplicate — same target column.
                t.label()
                    .copy_from(InsertSelectSource::literal("override".to_string())),
            ]
        })
        .execute(&mut ctx)
        .await
        .unwrap_err();
    assert!(
        matches!(err, DjogiError::Validation(ref msg) if msg.contains("'label'") && msg.contains("more than once")),
        "expected Validation error citing duplicate column 'label', got: {err:?}",
    );
}

#[djogi::djogi_test(sync_models = [Phase85C4bSource, Phase85C4bArchive])]
async fn insert_select_rejects_distinct_source(mut ctx: djogi::DjogiContext) {
    // `.distinct()` on the source — rejected by the v0.1 surface.
    // Future work can lift this with an explicit opt-in.
    let _seeded = seed_sources(&mut ctx).await;

    let err = Phase85C4bSource::objects()
        .distinct()
        .insert_into::<Phase85C4bArchive, _, _>(|t, s| {
            vec![t.label().copy_from(s.label().as_insert_source())]
        })
        .execute(&mut ctx)
        .await
        .unwrap_err();
    assert!(
        matches!(err, DjogiError::Validation(ref msg) if msg.contains(".distinct")),
        "expected Validation error for .distinct() on source, got: {err:?}",
    );
}
