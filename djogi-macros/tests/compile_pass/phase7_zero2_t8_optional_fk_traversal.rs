//! Phase 7-Zero-2 T8 — optional-FK forward traversal on visage-scoped Fields.
//!
//! `author: Option<ForeignKey<User>>` with `expose(public -> UserPublic)` emits
//! a traversal accessor that returns `OptionalRelationRef<UserPublicFields>`
//! — the nullability is honoured at the type level. The caller composes an
//! inner closure through `.map_filter(|a| …)`; the emitter guards the
//! resulting SQL with `author IS NOT NULL AND <inner>`.
//!
//! This fixture pins:
//!
//! 1. The accessor's return type is `OptionalRelationRef<V>` where `V` is the
//!    peer's `Fields` struct (not the peer's visage directly).
//! 2. `.map_filter(|a| a.display_name().eq(…))` composes a `Condition`.
//! 3. `.is_none()` / `.is_some()` standalone predicates compose.
//!
//! ## Deferred: `PostPublic::filter(|p| …)` entry point
//!
//! Like the required-FK fixture, we prove the chain through a standalone
//! helper rather than through `::filter` (which lands in T10). The
//! helper's body is the T8 acceptance criterion for the optional path.

use djogi::prelude::*;
use djogi::query::OptionalRelationRef;
use djogi::query::internal::Condition;

#[model(table = "phase7_zero2_t8_opt_users")]
#[derive(Debug, Clone)]
pub struct User {
    #[field(expose(public))]
    pub display_name: String,
}

#[model(table = "phase7_zero2_t8_opt_posts", no_default)]
#[derive(Debug, Clone)]
pub struct Post {
    #[field(expose(public))]
    pub title: String,
    // Nullable FK — exposure-relation form is lifted post-T6.
    #[field(expose(public -> UserPublic))]
    pub author: Option<ForeignKey<User>>,
}

/// Optional-FK traversal under the wrapper's `map_filter` combinator.
/// The emitted SQL guards on `author IS NOT NULL` before applying the
/// inner closure's condition — that is the T8 acceptance shape.
#[allow(dead_code)]
fn optional_traversal_composes(p: &PostPublicFields) -> Condition {
    p.author()
        .map_filter(|a| a.display_name().eq("Ada".to_string()))
}

/// Standalone `IS NULL` / `IS NOT NULL` predicates over the wrapper.
/// Useful when the caller wants to match "rows with no author" or
/// "rows with any author" without composing an inner closure.
#[allow(dead_code)]
fn optional_presence_composes(p: &PostPublicFields) -> (Condition, Condition) {
    let author = p.author();
    let author_again = p.author();
    (author.is_some(), author_again.is_none())
}

fn main() {
    let fields = PostPublicFields::default();

    // The accessor's static return type is `OptionalRelationRef<UserPublicFields>`.
    // Name the type to pin the contract — if the emitter ever regresses to
    // returning the bare peer `Fields`, this binding fails.
    let _opt: OptionalRelationRef<UserPublicFields> = fields.author();

    // Scalar on the owning visage still composes alongside.
    let _own: Condition = fields.title().eq("Hello".to_string());

    // The map_filter + presence helpers type-check.
    let _mapped: Condition = optional_traversal_composes(&fields);
    let (_some, _none) = optional_presence_composes(&fields);
}
