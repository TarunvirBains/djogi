//! Single-model adopter model crate — the SEPARATE crate whose link
//! retention T-LINK/T-DROPGUARD prove. Because it is its own crate (not a
//! module), the linker can drop it entirely when `bin` references nothing
//! from it — the partial-linkage hazard the unforced fixture reproduces.
use djogi::prelude::*;

#[derive(Model)]
#[model(table = "invoices")]
pub struct Invoice {
 pub reference: String,
}
