// F1 — the `__DJOGI_PK_SEAL_TOKEN` constant used to live at
// `djogi::primary_key::__DJOGI_PK_SEAL_TOKEN`. That public path made
// the seal bypassable: downstream code could grab the witness and
// hand-roll `impl PrimaryKey for SomeType`, slipping a non-blessed
// type past the closed-world contract.
//
// The token now lives only under `djogi::__private::pk_seal::TOKEN`,
// the off-limits framework-private path. Reaching for the old public
// path is a compile error.
use djogi::primary_key::{PkSealToken, PrimaryKey};
use djogi::PkType;

pub struct FakePk;

impl PrimaryKey for FakePk {
 const __DJOGI_PK_SEAL: PkSealToken = djogi::primary_key::__DJOGI_PK_SEAL_TOKEN;
 const KIND: PkType = PkType::Serial;
 const SQL_TYPE: &'static str = "BIGINT";
 const DEFAULT_SQL: Option<&'static str> = None;

 fn sentinel() -> Self {
  Self
 }
}

fn main() {}
