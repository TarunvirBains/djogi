// Phase 7.5 T3 rule (c) — `codec = "<id>"` must reference an ID that
// appears in the framework's compile-time codec registry. V1 ships
// the registry empty; every codec literal is unregistered and the
// macro lists "(none)" as the valid set so adopters know support
// has not yet shipped.
//
// The synchronization contract on `djogi::field_codec::REGISTRY`
// pins both the macro-side `KNOWN_CODEC_IDS` slice and the runtime
// `phf::Set` together — this fixture catches the V1 empty state.
use djogi::prelude::*;

#[model(table = "users")]
#[derive(Debug, Clone)]
pub struct User {
    #[field(protected(
        sensitivity = "pii",
        rationale = "Email confirmation flow",
        codec = "aes256_gcm_v1"
    ))]
    pub email: String,
}

fn main() {}
