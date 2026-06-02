//! `OnDelete` — cascade semantics parsed from `#[field(on_delete = "...")]`
//! and emitted into DDL by the migration layer .
//! Parses and stores the value on `FieldDescriptor`; DDL
//! emission of the FK constraint happens when migrations land in .
//! Does *not* emit `ON DELETE ...` in any runtime-issued SQL
//! FK enforcement is entirely a schema concern once the constraint is
//! in place, and driving it from runtime would duplicate the contract
//! badly.
//! # Why the default is `Restrict`
//! `CLAUDE.md` pins the framework default to `RESTRICT`: cascades are
//! dangerous-by-default and have to be opted into per field. Users
//! who want `CASCADE` write `#[field(on_delete = "cascade")]` at the
//! field; otherwise the migration layer emits `ON DELETE RESTRICT`.
//! This also matches the Django posture (`on_delete` is required with
//! no implicit fallback) without forcing every relation field to
//! restate the common case.

/// Foreign-key `ON DELETE` behavior.
/// `#[non_exhaustive]` so we can grow this (e.g. a future
/// `RestrictIfReferenced` that differs in error messaging) without
/// breaking downstream matches. See `CLAUDE.md` for the project
/// stance on `#[non_exhaustive]` on all public enums.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OnDelete {
    /// `ON DELETE CASCADE` — deleting the parent row deletes every
    /// row that references it. Opt-in because cascading deletes can
    /// destroy a surprising amount of data in a single statement.
    Cascade,

    /// `ON DELETE RESTRICT` — deleting the parent row fails with a
    /// foreign-key violation if any child references it. The default.
    Restrict,

    /// `ON DELETE SET NULL` — sets the child's relation column to
    /// `NULL`. Only legal when the relation column is nullable; the
    /// migration differ cross-checks this in .
    SetNull,

    /// `ON DELETE SET DEFAULT` — sets the child's relation column to
    /// the column's `DEFAULT`. The column must have one; again the
    /// differ validates.
    SetDefault,

    /// Djogi-specific alias for `Restrict` that signals *intent*: the
    /// child row is "protected" — the application considers it a hard
    /// error to try to delete the parent while children remain. Emits
    /// `RESTRICT` DDL today; a future phase may add a
    /// `DjogiError::Protected` variant distinct from the generic
    /// Postgres FK violation. Keeps user code readable at the
    /// `#[field]` attribute without forcing them to learn a new SQL
    /// error path.
    Protect,

    /// `ON DELETE NO ACTION` — the historical Postgres default.
    /// Behaves like `RESTRICT` for `IMMEDIATE` constraints but
    /// defers the check to transaction commit for `DEFERRABLE`
    /// constraints. Included for completeness; the default
    /// constraints Djogi emits are `IMMEDIATE`, so most users want
    /// `Restrict` instead.
    DoNothing,
}

impl Default for OnDelete {
    /// `Restrict` — cascade-by-default is a scalability and safety
    /// hazard (see the module docs).
    fn default() -> Self {
        OnDelete::Restrict
    }
}

impl OnDelete {
    /// DDL fragment — used by the migration layer in . Not
    /// used in any runtime query path.
    /// `Protect` aliases to `RESTRICT` at the SQL level — the alias
    /// only carries intent through the descriptor for future tooling
    /// (e.g. better error messages). Keep that mapping stable; any
    /// change would be a silent schema migration for every relation
    /// using `Protect`.
    pub fn as_sql(self) -> &'static str {
        match self {
            OnDelete::Cascade => "CASCADE",
            OnDelete::Restrict => "RESTRICT",
            OnDelete::SetNull => "SET NULL",
            OnDelete::SetDefault => "SET DEFAULT",
            OnDelete::Protect => "RESTRICT",
            OnDelete::DoNothing => "NO ACTION",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn on_delete_default_is_restrict() {
        assert_eq!(OnDelete::default(), OnDelete::Restrict);
    }

    #[test]
    fn on_delete_as_sql_per_variant() {
        assert_eq!(OnDelete::Cascade.as_sql(), "CASCADE");
        assert_eq!(OnDelete::Restrict.as_sql(), "RESTRICT");
        assert_eq!(OnDelete::SetNull.as_sql(), "SET NULL");
        assert_eq!(OnDelete::SetDefault.as_sql(), "SET DEFAULT");
        assert_eq!(OnDelete::DoNothing.as_sql(), "NO ACTION");
    }

    #[test]
    fn on_delete_protect_aliases_restrict() {
        // Intentional alias — locked here so accidental divergence
        // shows up as a test failure rather than silent DDL drift.
        assert_eq!(OnDelete::Protect.as_sql(), "RESTRICT");
    }
}
