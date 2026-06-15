// #171: typed array element support.
//
// Verifies that #[model] accepts Vec<V> for every element type in the
// expanded `IntoArrayFilterValue` sealed set: i16, f32, f64, DateTime,
// Date, uuid::Uuid, rust_decimal::Decimal, HeerId, RanjId,
// HeerIdRecencyBiased (= HeerIdDesc), and RanjIdRecencyBiased (= RanjIdDesc).
// The compile-pass fixture acts as the type-level gate; runtime operator
// behavior is tested in the integration suite.
//
// Array operators live on `ExplicitPgPredicateField` — they require the
// `.explicit_pg_predicate()` bridge from `DjogiField` because Postgres
// array predicates are not portable to Punnu in-memory evaluation.
use djogi::prelude::*;

#[model(table = "audit_logs", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct AuditLog {
 /// `Vec<HeerId>` — the canonical use-case from issue #171.
 pub touched_ids: Vec<djogi::HeerId>,
 pub ranj_ids: Vec<djogi::RanjId>,
 /// `Vec<HeerIdRecencyBiased>` (= `HeerIdDesc`) — newest-first BIGINT[].
 pub recency_ids: Vec<djogi::HeerIdRecencyBiased>,
 /// `Vec<RanjIdRecencyBiased>` (= `RanjIdDesc`) — newest-first UUID[].
 pub recency_ranj_ids: Vec<djogi::RanjIdRecencyBiased>,
 pub smallint_scores: Vec<i16>,
 pub float_scores: Vec<f32>,
 pub double_scores: Vec<f64>,
 pub event_times: Vec<djogi::DateTime>,
 pub event_dates: Vec<djogi::Date>,
 pub external_ids: Vec<uuid::Uuid>,
 pub amounts: Vec<rust_decimal::Decimal>,
 // Previously supported element types must still compile.
 pub labels: Vec<String>,
 pub flags: Vec<bool>,
 pub int_codes: Vec<i32>,
 pub big_codes: Vec<i64>,
}

fn _check_field_types(log: &AuditLog) {
 let _: &Vec<djogi::HeerId> = &log.touched_ids;
 let _: &Vec<djogi::RanjId> = &log.ranj_ids;
 let _: &Vec<djogi::HeerIdRecencyBiased> = &log.recency_ids;
 let _: &Vec<djogi::RanjIdRecencyBiased> = &log.recency_ranj_ids;
 let _: &Vec<i16> = &log.smallint_scores;
 let _: &Vec<f32> = &log.float_scores;
 let _: &Vec<f64> = &log.double_scores;
 let _: &Vec<djogi::DateTime> = &log.event_times;
 let _: &Vec<djogi::Date> = &log.event_dates;
 let _: &Vec<uuid::Uuid> = &log.external_ids;
 let _: &Vec<rust_decimal::Decimal> = &log.amounts;
 let _: &Vec<String> = &log.labels;
 let _: &Vec<bool> = &log.flags;
 let _: &Vec<i32> = &log.int_codes;
 let _: &Vec<i64> = &log.big_codes;
}

// Verify that the array operators type-check for the new element types.
// These do not execute against a DB — the test only asserts that Rust
// can resolve the method calls through `.explicit_pg_predicate()`, which
// is the required route to Postgres-specific array operators from the
// `DjogiField`-based filter closure.
fn _check_array_operators() {
 let ids: Vec<djogi::HeerId> = vec![];
 let _ = AuditLog::objects()
 .filter(|f| f.touched_ids().explicit_pg_predicate().contains(&ids));

 let rids: Vec<djogi::RanjId> = vec![];
 let _ = AuditLog::objects()
 .filter(|f| f.ranj_ids().explicit_pg_predicate().contained_by(&rids));

 // HeerIdRecencyBiased / RanjIdRecencyBiased (= HeerIdDesc / RanjIdDesc) —
 // must route through the BIGINT[] / UUID[] wire path respectively.
 let recency_ids: Vec<djogi::HeerIdRecencyBiased> = vec![];
 let _ = AuditLog::objects()
 .filter(|f| f.recency_ids().explicit_pg_predicate().contains(&recency_ids));

 let recency_ranj_ids: Vec<djogi::RanjIdRecencyBiased> = vec![];
 let _ = AuditLog::objects()
 .filter(|f| f.recency_ranj_ids().explicit_pg_predicate().overlap(&recency_ranj_ids));

 let scores: Vec<i16> = vec![1, 2, 3];
 let _ = AuditLog::objects()
 .filter(|f| f.smallint_scores().explicit_pg_predicate().overlap(&scores));

 let fscores: Vec<f32> = vec![1.0, 2.5];
 let _ = AuditLog::objects()
 .filter(|f| f.float_scores().explicit_pg_predicate().contains(&fscores));

 let dscores: Vec<f64> = vec![0.5, 9.9];
 let _ = AuditLog::objects()
 .filter(|f| f.double_scores().explicit_pg_predicate().contains(&dscores));

 let uuids: Vec<uuid::Uuid> = vec![];
 let _ = AuditLog::objects()
 .filter(|f| f.external_ids().explicit_pg_predicate().contains(&uuids));

 let amounts: Vec<rust_decimal::Decimal> = vec![];
 let _ = AuditLog::objects()
 .filter(|f| f.amounts().explicit_pg_predicate().contains(&amounts));

 // len() is on DjogiField directly (it returns Expr<i32>, which is not
 // a portability-gated predicate on its own).
 let _ = AuditLog::objects()
 .filter(|f| f.touched_ids().len().gt(0_i32));
}

fn main() {}
