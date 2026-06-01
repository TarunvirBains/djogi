//! Partial-linkage shape: references tracker only; the `billing` CRATE is
//! unreferenced, so the linker may drop it whole — the dangerous
//! silent-drop case T-LINK proves and the linkage guard (T-DROPGUARD)
//! must catch. NOT using djogi_main! here (the macro would force both);
//! hand-written so billing is genuinely unforced.
fn main() -> std::process::ExitCode {
    let _ = <tracker::Elephant as djogi::model::Model>::descriptor();
    // billing::Invoice deliberately NOT forced — its crate is dead-strippable.
    djogi_cli::run_from_env()
}
