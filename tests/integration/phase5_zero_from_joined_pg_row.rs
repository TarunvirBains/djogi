// Phase 5-Zero T4 — joined-row decode through the typed `select_related`
// surface on a foreign-key chain.

use djogi::prelude::*;

// Phase 7-Zero-2 T2 default flip — pin HeerId across the three linked
// models; the joined-decode test uses BIGINT `generate_id()` DDL and
// explicit HeerId wiring through FK columns.
#[model(table = "t4_chain_c", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct ChainC {
    pub label: String,
}

#[model(table = "t4_chain_b", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct ChainB {
    pub label: String,
    pub chain_c_id: ForeignKey<ChainC>,
}

#[model(table = "t4_chain_a", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct ChainA {
    pub label: String,
    pub chain_b_id: ForeignKey<ChainB>,
}

fn chain_b_for_insert(label: &str, chain_c: &ChainC) -> ChainB {
    ChainB {
        id: <djogi::types::HeerId as djogi::PrimaryKey>::sentinel(),
        created_at: djogi::types::DateTime::UNIX_EPOCH,
        updated_at: djogi::types::DateTime::UNIX_EPOCH,
        label: label.into(),
        chain_c_id: ForeignKey::new(chain_c.id),
    }
}

fn chain_a_for_insert(label: &str, chain_b: &ChainB) -> ChainA {
    ChainA {
        id: <djogi::types::HeerId as djogi::PrimaryKey>::sentinel(),
        created_at: djogi::types::DateTime::UNIX_EPOCH,
        updated_at: djogi::types::DateTime::UNIX_EPOCH,
        label: label.into(),
        chain_b_id: ForeignKey::new(chain_b.id),
    }
}

#[djogi::djogi_test(sync_models = [ChainC, ChainB, ChainA])]
async fn select_related_decodes_each_hop_in_fk_chain(mut ctx: djogi::DjogiContext) {
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

    let rows: Vec<JoinedRow<ChainA>> = ChainA::objects()
        .select_related(ChainARelated::chain_b())
        .filter(|f| f.id().eq(a.id))
        .fetch_all_joined(&mut ctx)
        .await
        .expect("ChainA select_related ChainB should succeed");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].row.id, a.id);
    assert_eq!(rows[0].row.label, "root");
    assert_eq!(rows[0].row.chain_b_id.key(), b.id);

    let joined_b = rows[0]
        .get(ChainARelated::chain_b())
        .expect("ChainB should be joined from ChainA");
    assert_eq!(joined_b.id, b.id);
    assert_eq!(joined_b.label, "middle");
    assert_eq!(joined_b.chain_c_id.key(), c.id);

    let rows: Vec<JoinedRow<ChainB>> = ChainB::objects()
        .select_related(ChainBRelated::chain_c())
        .filter(|f| f.id().eq(b.id))
        .fetch_all_joined(&mut ctx)
        .await
        .expect("ChainB select_related ChainC should succeed");

    assert_eq!(rows.len(), 1);
    let joined_c = rows[0]
        .get(ChainBRelated::chain_c())
        .expect("ChainC should be joined from ChainB");
    assert_eq!(joined_c.id, c.id);
    assert_eq!(joined_c.label, "leaf");
}
