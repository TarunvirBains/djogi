// `codec = "<id>"` must reference an ID that appears in the
// framework's compile-time codec registry. Using a codec ID not in
// `KNOWN_CODEC_IDS` triggers a compile-time error listing the valid
// IDs the build recognizes.
use djogi::prelude::*;

#[model(table = "users")]
#[derive(Debug, Clone)]
pub struct User {
    #[field(protected(
        sensitivity = "pii",
        rationale = "Email confirmation flow",
        codec = "unknown_codec_v1"
    ))]
    pub email: String,
}

fn main() {}
