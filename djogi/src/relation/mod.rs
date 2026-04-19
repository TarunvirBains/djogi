//! Relation field types and (later) relation-aware query extensions.
//!
//! Phase 3 Task 1 lands the runtime wrappers only:
//!
//! - [`ForeignKey<T>`] / [`ForeignKeyResolved<T>`] — many-to-one.
//! - [`OneToOneField<T>`] / [`OneToOneFieldResolved<T>`] —
//!   unique-constrained singular relation.
//! - [`OnDelete`] — cascade enum emitted into DDL by Phase 6's
//!   migration layer.
//!
//! Later Phase 3 tasks extend this module with:
//!
//! - `path.rs` / `RelationPath<Source, Target>` — typed ZST relation
//!   handle produced by `{Source}Related::relation_name()` for prefetch
//!   / select_related (Task 2).
//! - `prefetch.rs` / `PrefetchedRow<T>` — post-prefetch wrapper + its
//!   two-query stitching loader (Task 4).
//! - `joined_row.rs` / `JoinedRow<T>` + `FromJoinedRow` — post-select_related
//!   wrapper + prefix-aware row decoder (Task 5).
//! - `select_related.rs` — single-hop LEFT JOIN SQL emission + joined-
//!   row stitching glue (Task 5).
//! - `many_to_many.rs` / `ManyToMany<Target>` trait + through-model
//!   plumbing (Task 6).
//!
//! See `docs/guide/relations.md` (Phase 3 Task 8) for the user-facing
//! guide once later tasks land.

pub mod foreign_key;
pub mod joined_row;
pub mod on_delete;
pub mod one_to_one;
pub mod path;
pub mod prefetch;
pub mod select_related;

pub use foreign_key::{ForeignKey, ForeignKeyResolved};
pub use joined_row::{FromJoinedRow, JoinedRow};
pub use on_delete::OnDelete;
pub use one_to_one::{OneToOneField, OneToOneFieldResolved};
pub use path::{RelationKind, RelationPath};
pub use prefetch::PrefetchedRow;

/// Macro-only entry points. **Not** part of the stable public API.
///
/// `djogi-macros` emits calls into this module from user-crate code that
/// `#[derive(Model)]` expands — the items here are `pub` only so cross-crate
/// codegen can reach them. The double-underscore prefix and `#[doc(hidden)]`
/// marker signal to tooling and reviewers that downstream code must not
/// call these directly; the macro is the sole supported caller.
///
/// The seal exists to close the SQL-injection vector that was reachable when
/// [`RelationPath`]'s constructor was `pub`: a downstream caller could
/// previously fabricate a path whose identifier strings carried SQL
/// metacharacters, and those strings flowed through
/// [`sqlx::QueryBuilder::push`] in the prefetch / select_related emitters.
/// Constructing a path now requires going through
/// [`__make_relation_path`](__private::__make_relation_path), which enforces
/// the full Postgres unquoted-identifier rule: the first byte must match
/// `[A-Za-z_]`, remaining bytes must match `[A-Za-z0-9_]`, the total length
/// must not exceed Postgres's 63-byte `NAMEDATALEN` limit, and the string
/// must not be a reserved Postgres keyword (case-insensitive). Any identifier
/// that is not a valid *unquoted* Postgres identifier — even one that is
/// safe against classic SQL-injection metacharacters — is rejected, so the
/// emitter can never produce malformed SQL like `p.123` or `LEFT JOIN select ...`
/// from hostile downstream input.
#[doc(hidden)]
pub mod __private {
    use super::path::{RelationKind, RelationPath};
    use crate::model::Model;

    /// Postgres's `NAMEDATALEN` limit — identifiers longer than this are
    /// silently truncated by the server, which would let two Rust-distinct
    /// identifiers collide after the first 63 bytes. Rejecting up front
    /// keeps the Rust- and SQL-level identity contracts aligned.
    const MAX_IDENT_LEN: usize = 63;

    /// Postgres fully-reserved keywords (catcode `R` in `pg_get_keywords()`).
    /// These cannot be used as identifiers unless quoted, so emitting them
    /// unquoted into `SELECT`, `FROM`, or `LEFT JOIN` produces a parse
    /// error — turning a hostile downstream call into a broken-SQL vector.
    /// Entries are stored lowercase and kept lexicographically sorted so
    /// the check can use `binary_search`. Postgres identifier comparison is
    /// case-insensitive for unquoted identifiers, so callers must match
    /// against a lowercased copy of the input.
    const RESERVED_KEYWORDS: &[&str] = &[
        "all",
        "analyse",
        "analyze",
        "and",
        "any",
        "array",
        "as",
        "asc",
        "asymmetric",
        "both",
        "case",
        "cast",
        "check",
        "collate",
        "column",
        "constraint",
        "create",
        "current_catalog",
        "current_date",
        "current_role",
        "current_time",
        "current_timestamp",
        "current_user",
        "default",
        "deferrable",
        "desc",
        "distinct",
        "do",
        "else",
        "end",
        "except",
        "false",
        "fetch",
        "for",
        "foreign",
        "from",
        "grant",
        "group",
        "having",
        "in",
        "initially",
        "intersect",
        "into",
        "lateral",
        "leading",
        "limit",
        "localtime",
        "localtimestamp",
        "not",
        "null",
        "offset",
        "on",
        "only",
        "or",
        "order",
        "placing",
        "primary",
        "references",
        "returning",
        "select",
        "session_user",
        "some",
        "symmetric",
        "system_user",
        "table",
        "then",
        "to",
        "trailing",
        "true",
        "union",
        "unique",
        "user",
        "using",
        "variadic",
        "when",
        "where",
        "window",
        "with",
    ];

    /// Full runtime validation for a macro-emitted identifier. A panic
    /// (rather than `Result`) is appropriate here because reaching the
    /// helper with a bad identifier indicates either a broken proc-macro
    /// emission or a downstream caller deliberately bypassing the seal —
    /// both are framework-bug or misuse cases that the caller cannot
    /// recover from. The validator enforces four rules:
    ///
    /// 1. Non-empty.
    /// 2. Length ≤ `NAMEDATALEN` (63 bytes), so Rust- and Postgres-level
    ///    identifier identity cannot diverge through server-side
    ///    truncation.
    /// 3. First byte `[A-Za-z_]`, remaining bytes `[A-Za-z0-9_]` — the
    ///    Postgres unquoted-identifier grammar (Djogi additionally rejects
    ///    the `$` byte that Postgres tolerates, to keep the class trivial
    ///    to reason about).
    /// 4. Not a reserved Postgres keyword — rejected case-insensitively
    ///    against [`RESERVED_KEYWORDS`].
    #[inline]
    fn assert_plain_ident(value: &'static str, role: &'static str) {
        assert!(
            !value.is_empty(),
            "djogi::relation: macro-emitted {role} must not be empty — this is a framework bug"
        );
        assert!(
            value.len() <= MAX_IDENT_LEN,
            "djogi::relation: macro-emitted {role} {value:?} is {len} bytes, exceeding Postgres's \
             {max}-byte NAMEDATALEN limit — either the proc-macro emission is broken or downstream \
             code bypassed the RelationPath seal",
            len = value.len(),
            max = MAX_IDENT_LEN,
        );
        let bytes = value.as_bytes();
        let first = bytes[0];
        let first_ok = first.is_ascii_alphabetic() || first == b'_';
        assert!(
            first_ok,
            "djogi::relation: macro-emitted {role} {value:?} must start with a letter or \
             underscore — either the proc-macro emission is broken or downstream code bypassed \
             the RelationPath seal"
        );
        for &byte in &bytes[1..] {
            let ok = byte.is_ascii_alphanumeric() || byte == b'_';
            assert!(
                ok,
                "djogi::relation: macro-emitted {role} {value:?} contains a non-identifier \
                 character — either the proc-macro emission is broken or downstream code \
                 bypassed the RelationPath seal"
            );
        }
        // Case-insensitive reserved-keyword check. Allocating here is fine:
        // this runs once per `{Model}Related::relation_name()` call, which
        // in practice is once per prefetch/select_related path construction
        // (not per row, not per query). The small-string lowercase stays
        // on the stack for common identifier lengths.
        let lower = value.to_ascii_lowercase();
        assert!(
            RESERVED_KEYWORDS.binary_search(&lower.as_str()).is_err(),
            "djogi::relation: macro-emitted {role} {value:?} is a reserved Postgres keyword and \
             cannot appear unquoted in generated SQL — either the proc-macro emission is broken \
             or downstream code bypassed the RelationPath seal"
        );
    }

    /// Construct a [`RelationPath<Source, Target>`] from macro-emitted
    /// identifier strings. The only supported caller is the
    /// `{Source}Related::relation_name()` method that `#[derive(Model)]`
    /// emits in the user's crate.
    ///
    /// Panics if `source_column` or `target_table` violates any rule in
    /// [`assert_plain_ident`]: empty, over 63 bytes, leading digit, a byte
    /// outside `[A-Za-z0-9_]`, or a reserved Postgres keyword. The check
    /// is the runtime half of the seal; the compile-time half is
    /// [`RelationPath::new`] being `pub(crate)`.
    #[doc(hidden)]
    pub fn __make_relation_path<Source: Model, Target: Model>(
        source_column: &'static str,
        target_table: &'static str,
        kind: RelationKind,
    ) -> RelationPath<Source, Target> {
        assert_plain_ident(source_column, "source_column");
        assert_plain_ident(target_table, "target_table");
        RelationPath::new(source_column, target_table, kind)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::DjogiError;
        use crate::descriptor::ModelDescriptor;
        use crate::model::Model;
        use crate::types::HeerId;
        use std::future::Future;

        // Minimal inert `Model` stubs — enough to satisfy the `Model`
        // bounds on `RelationPath`'s generics. Mirrors the pattern in
        // `djogi/src/relation/path.rs`'s unit tests. The CRUD methods
        // panic if called; the validator tests below never exercise
        // them.
        struct Src;
        struct Dst;

        macro_rules! dummy_model {
            ($ty:ty, $table:literal) => {
                impl Model for $ty {
                    type Pk = HeerId;
                    type Fields = ();
                    fn table_name() -> &'static str {
                        $table
                    }
                    fn pk_value(&self) -> &Self::Pk {
                        unreachable!()
                    }
                    fn descriptor() -> &'static ModelDescriptor {
                        unreachable!()
                    }
                    fn get<'a>(
                        _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
                        _id: Self::Pk,
                    ) -> impl Future<Output = Result<Self, DjogiError>> + Send {
                        async { unreachable!() }
                    }
                    fn create<'a>(
                        _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
                        _v: Self,
                    ) -> impl Future<Output = Result<Self, DjogiError>> + Send {
                        async { unreachable!() }
                    }
                    fn save<'a>(
                        &self,
                        _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
                    ) -> impl Future<Output = Result<(), DjogiError>> + Send {
                        async { unreachable!() }
                    }
                    fn delete<'a>(
                        self,
                        _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
                    ) -> impl Future<Output = Result<(), DjogiError>> + Send {
                        async { unreachable!() }
                    }
                    fn refresh_from_db<'a>(
                        &self,
                        _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
                    ) -> impl Future<Output = Result<Self, DjogiError>> + Send {
                        async { unreachable!() }
                    }
                }
            };
        }

        dummy_model!(Src, "srcs");
        dummy_model!(Dst, "dsts");

        // Helper: attempt a path construction and return whether it
        // panicked. `catch_unwind` is fine on every std target djogi
        // targets (Linux, macOS, Windows with the default unwind
        // panic strategy).
        fn try_make(
            source_column: &'static str,
            target_table: &'static str,
        ) -> std::thread::Result<RelationPath<Src, Dst>> {
            std::panic::catch_unwind(|| {
                __make_relation_path::<Src, Dst>(
                    source_column,
                    target_table,
                    RelationKind::ForeignKey,
                )
            })
        }

        #[test]
        fn accepts_plain_identifier() {
            assert!(try_make("owner_id", "owners").is_ok());
        }

        #[test]
        fn accepts_identifier_with_trailing_digits() {
            assert!(try_make("col1", "t_abc123").is_ok());
        }

        #[test]
        fn rejects_empty_source_column() {
            assert!(try_make("", "owners").is_err());
        }

        #[test]
        fn rejects_empty_target_table() {
            assert!(try_make("owner_id", "").is_err());
        }

        #[test]
        fn rejects_leading_digit_source_column() {
            // Closes the pre-fix hole: `[A-Za-z0-9_]+` accepted "123",
            // which would emit `SELECT p.123 ...` or `LEFT JOIN ... ON
            // p.123 = ...`.
            assert!(try_make("123", "owners").is_err());
        }

        #[test]
        fn rejects_leading_digit_target_table() {
            assert!(try_make("owner_id", "9table").is_err());
        }

        #[test]
        fn rejects_reserved_keyword_source_column() {
            // "select" is catcode R in pg_get_keywords(); unquoted it
            // parses as the SELECT keyword and produces syntactically
            // invalid SQL anywhere an identifier is expected.
            assert!(try_make("select", "owners").is_err());
        }

        #[test]
        fn rejects_reserved_keyword_target_table() {
            assert!(try_make("owner_id", "table").is_err());
        }

        #[test]
        fn rejects_reserved_keyword_case_insensitively() {
            // Postgres folds unquoted identifiers to lowercase, so
            // "SELECT", "Select", "sElEcT" all reference the SELECT
            // keyword. The validator must match the same case rule.
            assert!(try_make("SELECT", "owners").is_err());
            assert!(try_make("Where", "owners").is_err());
        }

        #[test]
        fn rejects_identifier_exceeding_namedatalen() {
            // 64 bytes — one past Postgres's 63-byte NAMEDATALEN limit.
            const LONG: &str = "a234567890123456789012345678901234567890123456789012345678901234";
            assert_eq!(LONG.len(), 64);
            assert!(try_make(LONG, "owners").is_err());
        }

        #[test]
        fn accepts_identifier_at_namedatalen_limit() {
            // 63 bytes — exactly at the limit, must pass.
            const AT_LIMIT: &str =
                "a23456789012345678901234567890123456789012345678901234567890123";
            assert_eq!(AT_LIMIT.len(), 63);
            assert!(try_make(AT_LIMIT, "owners").is_ok());
        }

        #[test]
        fn rejects_metacharacter_payload() {
            // The original SQL-injection shape from the Task 4 seal
            // fixture. Rejected on the space/paren during the per-byte
            // character-class check.
            assert!(try_make("owner_id) OR 1=1 --", "owners").is_err());
        }

        #[test]
        fn reserved_keywords_list_is_sorted() {
            // The binary_search in `assert_plain_ident` assumes a sorted
            // slice. Guard against a later edit that adds keywords out
            // of order.
            for pair in RESERVED_KEYWORDS.windows(2) {
                assert!(
                    pair[0] < pair[1],
                    "RESERVED_KEYWORDS must be sorted for binary_search: {:?} !< {:?}",
                    pair[0],
                    pair[1],
                );
            }
        }
    }
}
