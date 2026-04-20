//! Using the relation form `expose(public = "WidgetSummary")` on a scalar
//! field must be rejected — `scope = "PeerType"` is only valid on relation
//! fields (`ForeignKey<T>` / `OneToOneField<T>`).
use djogi::prelude::*;

#[model(table = "widgets_expose_reform")]
#[derive(Debug, Clone)]
pub struct Widget {
    #[field(expose(public = "WidgetSummary"))]
    pub name: String,
}

fn main() {}
