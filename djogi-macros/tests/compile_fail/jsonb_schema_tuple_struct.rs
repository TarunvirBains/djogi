// #[derive(JsonbSchema)] on a tuple struct must be rejected at compile time.
use djogi::JsonbSchema;

#[derive(JsonbSchema)]
pub struct TupleSpecs(i32, f32);

fn main() {}
