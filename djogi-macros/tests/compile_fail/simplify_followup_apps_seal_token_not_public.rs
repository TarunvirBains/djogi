// F1 — the `__DJOGI_APPS_SEAL_TOKEN` constant used to live at
// `djogi::apps::__DJOGI_APPS_SEAL_TOKEN`. That public path made the
// seal bypassable: downstream code could grab the witness and
// hand-roll `impl App for SomeType`, slipping a non-`djogi::apps!`
// struct past the closed-world contract.
//
// The token now lives only under `djogi::__private::apps_seal::TOKEN`,
// the off-limits framework-private path. Reaching for the old public
// path is a compile error.
use djogi::apps::SealToken;
use djogi::App;

pub struct FakeApp;

impl App for FakeApp {
    const __DJOGI_APP_SEAL: SealToken = djogi::apps::__DJOGI_APPS_SEAL_TOKEN;
    const LABEL: &'static str = "fake";
    const DATABASE: &'static str = "main";
    const DESCRIPTOR: djogi::AppDescriptor = djogi::AppDescriptor {
        label: "fake",
        database: "main",
        renamed_from: None,
        tombstone: false,
    };
}

fn main() {}
