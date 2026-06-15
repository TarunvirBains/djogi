//! #231 — `DjogiVisage` is sealed against arbitrary downstream
//! impls via the metadata-only `djogi::__private::DjogiVisageSealed`
//! seal. A plain model cannot satisfy the supertrait without reaching
//! into `__private` (which is the convention-only boundary), so a
//! hand-rolled `impl DjogiVisage for MyModel { type Model = Self;... }`
//! fails to compile.
//!
//! # What this fixture proves and does not prove
//!
//! Rust does not provide a way to mark a trait "implementable only
//! inside this crate" when its supertrait must be reachable from a
//! separate proc-macro-emitting crate. `DjogiVisageSealed` is therefore
//! still nameable through `djogi::__private::DjogiVisageSealed` (the
//! `#[doc(hidden)] pub mod __private` convention boundary) — adopter
//! code that deliberately reaches into `__private` to hand-roll the
//! seal is breaking the documented framework boundary and the
//! framework reserves the right to break that code in any future
//! release without notice.
//!
//! This fixture pins the **public-surface** seal: a model declaration
//! using only `djogi::prelude::*` (the canonical public surface) cannot
//! impl `DjogiVisage` for itself. That is the seal the public docs
//! claim closes the world on macro-emitted visages. The reflexive
//! `impl<M: Model> DjogiVisageOf<M> for M` blanket on the pairing seal
//! alone would otherwise let this slip through.

use djogi::DjogiVisage;
use djogi::prelude::*;

#[model(table = "visage_seal_downstream_impl_rejected_plain", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone, PartialEq)]
pub struct PlainModel {
 pub name: String,
}

// SHOULD FAIL — `PlainModel` does not impl `DjogiVisageSealed` (no
// reflexive blanket, and the `#[model]` macro only emits the seal for
// the four generated visage structs — `PlainModelPublic`,
// `PlainModelSelfView`, `PlainModelAdmin`, `PlainModelExport` — not for
// the source model itself).
impl DjogiVisage for PlainModel {
 type Model = Self;
 const SCOPE: &'static str = "fake";
 const COLUMNS: &'static [&'static str] = &[];
 const PROJECTIONS: &'static [djogi::__private::ProjectionEntry] = &[];
 const PROJECTION_LIST: &'static str = "";
}

fn main() {}
