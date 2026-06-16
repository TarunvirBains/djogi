use djogi::prelude::*;

#[model(table = "joined_debug_guard_posts", pk = HeerId)]
#[derive(Debug, Clone, PartialEq)]
pub struct DebugGuardPost {
    pub title: String,
    pub body: String,
    pub published: bool,
    pub view_count: i32,
}

#[cfg(debug_assertions)]
#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#475): no typed helper for constructing rows with controlled column aliases;
// raw SQL builds a synthetic row without the expected __djogi_old__ prefix to trigger the debug guard
#[djogi::djogi_test(sync_models = [DebugGuardPost])]
async fn from_joined_pg_row_debug_guard_rejects_missing_old_alias(mut ctx: djogi::DjogiContext) {
    let rows = ctx
        .raw_rows(
            "SELECT \
                1::bigint AS id, \
                now() AS created_at, \
                now() AS updated_at, \
                'Guarded Title'::text AS title, \
                'Guarded Body'::text AS body, \
                false AS published, \
                5::integer AS view_count",
            &[],
        )
        .await
        .expect("row should be constructed for debug-guard test");
    let row = rows.into_iter().next().expect("exactly one row");

    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        <DebugGuardPost as djogi::FromJoinedPgRow>::from_joined_pg_row(&row, "__djogi_old__")
    }));

    match caught {
        Ok(Ok(_)) => panic!("expected FromJoinedPgRow debug guard to panic, but decode succeeded"),
        Ok(Err(err)) => {
            panic!("expected FromJoinedPgRow debug guard to panic, got decode error: {err:?}")
        }
        Err(payload) => {
            let message = match payload.downcast_ref::<String>() {
                Some(msg) => msg.as_str(),
                None => match payload.downcast_ref::<&str>() {
                    Some(msg) => msg,
                    None => panic!("debug guard panic payload should be a string"),
                },
            };

            assert!(
                message.contains("FromJoinedPgRow"),
                "panic should mention FromJoinedPgRow, got: {message}"
            );
            assert!(
                message.contains("__djogi_old__"),
                "panic should mention the prefix, got: {message}"
            );
            assert!(
                message.contains("o0"),
                "panic should mention the missing alias, got: {message}"
            );
        }
    }
}

#[cfg(not(debug_assertions))]
#[test]
fn from_joined_pg_row_debug_guard_release_noop() {}
