//! Phase 7-Zero-2 T6 — `expose(scope -> Peer)` on `Option<ForeignKey<T>>`.
//!
//! T6 lifts the prior Phase 4.5 deferral that rejected relation-form
//! exposures on optional FKs. The new emission produces
//! `pub field: Option<PeerVisage>` and threads the resolved relation
//! through the peer's `TryFrom<&Target>` only when `Some`, returning
//! `None` for absent relations.
use djogi::prelude::*;

#[model(table = "users_t6_opt_arrow")]
#[derive(Debug, Clone)]
pub struct User {
    #[field(expose(public))]
    pub display_name: String,
}

#[model(table = "posts_t6_opt_arrow", no_default)]
#[derive(Debug, Clone)]
pub struct Post {
    #[field(expose(public))]
    pub title: String,

    // Optional FK in relation-form expose — previously compile-fail in
    // Phase 4.5, now emits `Option<UserPublic>` honestly at the type level.
    #[field(expose(public -> UserPublic))]
    pub author: Option<ForeignKey<User>>,
}

fn main() {
    // PostPublic must carry an `Option<UserPublic>` author.
    let _build = |post: &Post| -> Result<PostPublic, djogi::VisageError> {
        PostPublic::try_from(post)
    };
}
