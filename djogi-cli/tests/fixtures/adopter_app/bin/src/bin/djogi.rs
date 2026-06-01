//! Adopter `djogi` binary. Uses djogi_main! (the sugar) so the fixture
//! exercises the macro (T-POS + macro path). Lists at least one type per
//! model CRATE — tracker AND billing — so both crates' inventory statics
//! are retained.
djogi_cli::djogi_main!(
    tracker::Elephant,
    tracker::Herd,
    billing::Invoice
);
