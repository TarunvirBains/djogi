//! Billing crate — defines the Invoice model.
//!
//! Single-model crate used to test whether referencing NOTHING from billing
//! causes its inventory submissions to be dropped by the linker.

use djogi::prelude::*;

/// An invoice model — sole model in this crate.
#[model(table = "billing_invoice")]
pub struct Invoice {
    pub amount: i64,
    pub description: String,
}
