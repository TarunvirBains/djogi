//! ACCEPTANCE: relation embeddings resolve through the canonical-name
//! alias, so renaming a peer model's visage requires zero edits at the
//! embedding site.
//!
//! `Post` embeds `User`'s public visage as `UserPublic` (the canonical
//! name). `User` renames its public visage to `UserSummary`. `Post`'s
//! annotation is identical to what it would be if `User` had never been
//! renamed — that is the no-churn guarantee.
use djogi::prelude::*;
use djogi::query::internal::Condition;

#[model(table = "visage_rename_embedded_user", visage_names(public = UserSummary))]
#[derive(Debug, Clone)]
pub struct User {
    #[field(expose(public))]
    pub display_name: String,
}

#[model(table = "visage_rename_embedded_post", no_default)]
#[derive(Debug, Clone)]
pub struct Post {
    #[field(expose(public))]
    pub title: String,

    // Written against the canonical name. NOT edited for the rename.
    #[field(expose(public -> UserPublic))]
    pub author: ForeignKey<User>,
}

/// The embedding traversal composes through the peer's canonical `Fields`
/// alias even though the peer renamed its public visage. `PostPublic` is
/// relation-nesting (no `::filter`), so the chain is proven through a
/// standalone helper over the embedder's `Fields`. The embedder's own
/// `Fields` type uses the ELIDED generic form (`&PostPublicFields`, no
/// `<Post>`) because `{Visage}Fields` defaults `RootModel = Source` — the
/// corpus convention (`required_fk_traversal.rs:44`,
/// `optional_fk_traversal.rs:50`).
#[allow(dead_code)]
fn embeds_through_alias(p: &PostPublicFields) -> Condition {
    p.author().display_name().eq("Ada".to_string())
}

fn main() {
    let fields = PostPublicFields::default();

    // The accessor for the embedded relation types as the peer's CANONICAL
    // `Fields` alias parameterized by the EMBEDDER root (`UserPublicFields<Post>`),
    // which resolves through the rename to `UserSummaryFields<Post>`. The
    // explicit `<Post>` is REQUIRED here (not elided): the narrow accessor
    // emits `-> {Peer}Fields<{Root}>` with the embedder as the root
    // (`visage_fields.rs:244`), so the peer `Fields` is rooted on `Post`, NOT
    // on its own default `RootModel = User`. The corpus keeps this binding
    // explicit for the same reason (`optional_fk_traversal.rs:79`). This
    // binding is the acceptance assertion: the embedding site names only
    // `UserPublic`-derived types, and they all resolve unedited after `User`
    // renamed its public visage.
    let _peer_fields: UserPublicFields<Post> = fields.author();

    // The traversal chain type-checks into a `Condition`.
    let _embedded: Condition = embeds_through_alias(&fields);
}
