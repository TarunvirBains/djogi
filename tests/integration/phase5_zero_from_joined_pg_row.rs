//! Phase 5-Zero T4 — `FromJoinedPgRow` prefix-aware decode on a
//! three-level foreign-key chain.
//!
//! The Phase 3 `select_related` runtime is still single-hop, but the
//! trait itself is prefix-keyed and must decode any joined row whose
//! aliases follow the caller's chosen prefix convention. This test
//! builds a hand-rolled `A -> B -> C` join with aliased columns for
//! both descendant levels and asserts that the parent, middle, and leaf
//! all decode correctly via `FromJoinedPgRow::from_joined_pg_row`.

use djogi::prelude::*;

#[model(table = "t4_chain_c")]
#[derive(Debug, Clone)]
pub struct ChainC {
    pub label: String,
}

#[model(table = "t4_chain_b", no_default)]
#[derive(Debug, Clone)]
pub struct ChainB {
    pub label: String,
    pub chain_c_id: ForeignKey<ChainC>,
}

#[model(table = "t4_chain_a", no_default)]
#[derive(Debug, Clone)]
pub struct ChainA {
    pub label: String,
    pub chain_b_id: ForeignKey<ChainB>,
}

async fn setup_tables(ctx: &mut djogi::DjogiContext) {
    ctx.__execute_for_macros(
        "CREATE TABLE IF NOT EXISTS t4_chain_c (
            id         BIGINT      PRIMARY KEY DEFAULT generate_id(),
            created_at TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at TIMESTAMPTZ NOT NULL    DEFAULT now(),
            label      TEXT        NOT NULL
        )",
        &[],
    )
    .await
    .expect("create t4_chain_c");

    ctx.__execute_for_macros(
        "CREATE TABLE IF NOT EXISTS t4_chain_b (
            id         BIGINT      PRIMARY KEY DEFAULT generate_id(),
            created_at TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at TIMESTAMPTZ NOT NULL    DEFAULT now(),
            label      TEXT        NOT NULL,
            chain_c_id BIGINT      NOT NULL REFERENCES t4_chain_c(id) ON DELETE CASCADE
        )",
        &[],
    )
    .await
    .expect("create t4_chain_b");

    ctx.__execute_for_macros(
        "CREATE TABLE IF NOT EXISTS t4_chain_a (
            id         BIGINT      PRIMARY KEY DEFAULT generate_id(),
            created_at TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at TIMESTAMPTZ NOT NULL    DEFAULT now(),
            label      TEXT        NOT NULL,
            chain_b_id BIGINT      NOT NULL REFERENCES t4_chain_b(id) ON DELETE CASCADE
        )",
        &[],
    )
    .await
    .expect("create t4_chain_a");
}

fn chain_b_for_insert(label: &str, chain_c: &ChainC) -> ChainB {
    ChainB {
        id: djogi::types::__heerid_default(),
        created_at: djogi::types::DateTime::UNIX_EPOCH,
        updated_at: djogi::types::DateTime::UNIX_EPOCH,
        label: label.into(),
        chain_c_id: ForeignKey::new(chain_c.id),
    }
}

fn chain_a_for_insert(label: &str, chain_b: &ChainB) -> ChainA {
    ChainA {
        id: djogi::types::__heerid_default(),
        created_at: djogi::types::DateTime::UNIX_EPOCH,
        updated_at: djogi::types::DateTime::UNIX_EPOCH,
        label: label.into(),
        chain_b_id: ForeignKey::new(chain_b.id),
    }
}

#[djogi::djogi_test]
async fn from_joined_pg_row_decodes_three_level_fk_chain(mut ctx: djogi::DjogiContext) {
    setup_tables(&mut ctx).await;

    let c = ChainC::create(
        &mut ctx,
        ChainC {
            label: "leaf".into(),
            ..Default::default()
        },
    )
    .await
    .expect("create ChainC");

    let b = ChainB::create(&mut ctx, chain_b_for_insert("middle", &c))
        .await
        .expect("create ChainB");

    let a = ChainA::create(&mut ctx, chain_a_for_insert("root", &b))
        .await
        .expect("create ChainA");

    let row = ctx
        .__query_one_for_macros(
            "SELECT
                a.id,
                a.created_at,
                a.updated_at,
                a.label,
                a.chain_b_id,
                b.id AS \"rel_chain_b_id.id\",
                b.created_at AS \"rel_chain_b_id.created_at\",
                b.updated_at AS \"rel_chain_b_id.updated_at\",
                b.label AS \"rel_chain_b_id.label\",
                b.chain_c_id AS \"rel_chain_b_id.chain_c_id\",
                c.id AS \"rel_chain_b_id.rel_chain_c_id.id\",
                c.created_at AS \"rel_chain_b_id.rel_chain_c_id.created_at\",
                c.updated_at AS \"rel_chain_b_id.rel_chain_c_id.updated_at\",
                c.label AS \"rel_chain_b_id.rel_chain_c_id.label\"
             FROM t4_chain_a a
             JOIN t4_chain_b b ON a.chain_b_id = b.id
             JOIN t4_chain_c c ON b.chain_c_id = c.id
             WHERE a.id = $1",
            &[&a.id],
        )
        .await
        .expect("joined select should succeed");

    let decoded_a = <ChainA as djogi::pg::decode::FromJoinedPgRow>::from_joined_pg_row(&row, "")
        .expect("parent decode should succeed");
    let decoded_b =
        <ChainB as djogi::pg::decode::FromJoinedPgRow>::from_joined_pg_row(&row, "rel_chain_b_id.")
            .expect("middle decode should succeed");
    let decoded_c = <ChainC as djogi::pg::decode::FromJoinedPgRow>::from_joined_pg_row(
        &row,
        "rel_chain_b_id.rel_chain_c_id.",
    )
    .expect("leaf decode should succeed");

    assert_eq!(decoded_a.id, a.id);
    assert_eq!(decoded_a.label, "root");
    assert_eq!(decoded_a.chain_b_id.key(), b.id);

    assert_eq!(decoded_b.id, b.id);
    assert_eq!(decoded_b.label, "middle");
    assert_eq!(decoded_b.chain_c_id.key(), c.id);

    assert_eq!(decoded_c.id, c.id);
    assert_eq!(decoded_c.label, "leaf");
}
