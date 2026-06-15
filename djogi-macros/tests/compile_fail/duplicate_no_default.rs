// Duplicate `no_default` flags in a single #[model(...)] must be rejected
// with a span-carrying error, matching the existing duplicate-detection on
// `unique` / `index` in `FieldAttrs::parse`.
use djogi::prelude::*;

#[model(table = "x", no_default, no_default)]
pub struct Bad {
 pub name: String,
}

fn main() {}
