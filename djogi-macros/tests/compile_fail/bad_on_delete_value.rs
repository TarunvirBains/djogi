// An `on_delete = "..."` value outside the accepted set must be rejected
// with an error whose caret lands on the offending literal, not the whole
// field. The span-precision fix in 326bacd rewrites `FieldAttrs::parse` to
// walk the raw `#[field(...)]` attrs and recover the literal's `Span` after
// darling has already reduced the attribute to a plain `String`. This
// fixture pins that behaviour so any future darling upgrade, FieldAttrs
// refactor, or attr-walker rewrite that regresses the span fails loudly
// in the lihaaf suite instead of silently degrading the UX.
//
// Accepted values (see `FieldAttrs::parse`): cascade, restrict, set_null,
// set_default, protect, do_nothing. `"bogus"` is not in that set.
use djogi::prelude::*;

#[model(table = "comments")]
#[derive(Debug, Clone)]
pub struct Comment {
 #[field(on_delete = "bogus")]
 pub post_id: i64,
 pub body: String,
}

fn main() {}
