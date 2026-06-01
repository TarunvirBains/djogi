//! Documentation-only marker — `generated_column_replacement` does
//! not ship as a pattern.
//! # Why this module exists
//! A reader scanning the [`patterns`](super) directory might
//! reasonably expect a `generated_column_replacement.rs` next to
//! [`replacement_column`](super::replacement_column) — the obvious
//! pattern would shadow a stored generated column with a new one
//! carrying the new expression, swap reads, drop the old. That
//! pattern does not exist, and v3 plan §7 (amended 2026-04-28)
//! routes "Change stored generated column expression on populated
//! table" to
//! [`OnlineSafetyClassification::OfflineOnly`](crate::migrate::OnlineSafetyClassification::OfflineOnly)
//! rather than ExpandContract.
//! # The reasoning
//! Adding a stored generated column rewrites the table under
//! `AccessExclusiveLock` — Postgres evaluates the generation
//! expression for every row and writes the result to disk before
//! the `ALTER TABLE` returns. That lock window is the same one a
//! shadow-column pattern is meant to avoid, so a "shadow generated
//! column + swap" approach offers no relief: the row rewrite still
//! happens, just in a column with a different name.
//! Operators with an online-rollout requirement remodel the column
//! away from `STORED GENERATED` entirely (e.g. into a regular column
//! populated by an application trigger or a materialised value), at
//! which point the change classifies as a "Change column type
//! requires rewrite" and routes through
//! [`replacement_column`](super::replacement_column) instead. The
//! v3 plan documents this reroute in the "remodel away from STORED-
//! generated" amendment note on the same row.
//! # No `Pattern` impl
//! This module is intentionally empty of types and functions. It
//! exists as a breadcrumb so the [`patterns`](super) directory
//! listing carries a record of the deliberate omission, and so a
//! future contributor reading the directory can find the rationale
//! without having to recover it from the v3 plan amendment history.

#[cfg(test)]
mod tests {
    /// The module ships with no public API — this test pins the
    /// invariant that the file remains a documentation breadcrumb,
    /// not a hidden code surface. If a contributor adds a `Pattern`
    /// impl here later, this test will not catch it (Rust has no
    /// "this module exports nothing" assertion); the safeguard is
    /// the module-level doc comment plus the
    /// [`super::tests::dispatch_witnesses_pattern_id_uniqueness`]
    /// catalogue check, which would surface the new pattern's ID.
    #[test]
    fn module_is_documentation_only() {
        // No-op assertion. The intent here is a marker test that
        // documents *why* the module is empty so a developer
        // grepping for `generated_column_refusal` finds both the
        // documentation breadcrumb and an active test row that
        // anchors the decision in the test suite.
        let module_id = "generated_column_refusal";
        assert_eq!(module_id, "generated_column_refusal");
    }
}
