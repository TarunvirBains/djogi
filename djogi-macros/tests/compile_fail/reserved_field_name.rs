// Declaring `created_at` on a #[model] struct must fail with a targeted macro
// error — the macro always injects that field and a user redefinition would
// collide.
use djogi::prelude::*;

#[model(table = "posts")]
struct Bad {
 pub title: String,
 pub created_at: String,
}

fn main() {}
