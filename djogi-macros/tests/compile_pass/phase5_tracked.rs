// Phase 5 Task 1: Tracked<T> composes with primitive, collection, and wrapper types.
//
// Jsonb<T> composition (Tracked<Jsonb<T>>) lands in Phase 5 Task 5 once Jsonb<T>
// is introduced; this fixture stays on primitives + stdlib collections so it
// compiles independently of later tasks.
use djogi::prelude::*;

fn _check_string() {
    let s: Tracked<String> = Tracked::new("alice".to_string());
    assert!(!s.is_dirty());
    let _: &str = s.as_str(); // Deref to String → &str
}

fn _check_vec() {
    let v: Tracked<Vec<i32>> = Tracked::new(vec![1, 2, 3]);
    assert!(!v.is_dirty());
    let _: usize = v.len(); // Deref to Vec<i32>
}

fn _check_option() {
    let o: Tracked<Option<i64>> = Tracked::new(Some(42));
    assert!(!o.is_dirty());
}

fn _check_default() {
    // Tracked<T> is Default when T: Default, so ..Account::default()-style
    // struct-update syntax works in user model code.
    let _: Tracked<String> = Tracked::default();
    let _: Tracked<i64> = Tracked::default();
}

fn _check_mutation_dirties() {
    let mut t: Tracked<i64> = Tracked::new(0);
    *t += 1;
    assert!(t.is_dirty());
}

fn main() {}
