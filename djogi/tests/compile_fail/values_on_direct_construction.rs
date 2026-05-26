//! Verify that downstream code cannot forge `ValuesOn<T>` directly. The only
//! supported constructors are the typed `eq_values` helpers plus `&`
//! composition.
use djogi::prelude::*;

#[model(table = "animals", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct Animal {
    pub name: String,
}

fn main() {
    let _bad: ValuesOn<Animal> = ValuesOn::Eq {
        model_col: "id",
        values_col_idx: 0,
        _phantom: std::marker::PhantomData,
    };
}
