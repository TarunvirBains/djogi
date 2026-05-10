# Cluster 6 Docs Sweep — Pass 1: API Coverage (2026-05-10)

> Self-review only. Independent GPT-5.5 xhigh review pending.

## Method

Ran `cargo rustdoc -p <crate> --lib --all-features -- -W missing_docs` (and
`--bins` for `djogi-cli`, since it has no library target) against every
workspace crate at HEAD `63c2ce9`. Captured stderr and tabulated the warnings
by source file, kind, and adopter exposure. For each significant warning
location I read the surrounding source to classify the item as **public-API**
(re-exported through the crate root or `prelude`), **public-but-internal**
(`pub` for cross-module access but never exposed at the crate root), or
**third-party-style** (a re-export from `tokio_postgres` / `heeranjid`).
Workspace crates surveyed: `djogi`, `djogi-macros`, `djogi-cli`, `djogi-shell`.
The `djogi-maahi` crate referenced in `CLAUDE.md` does not yet exist on disk.

## Crate-level summary

| Crate | Missing-docs warnings | Notes |
|---|---|---|
| `djogi` | 420 | Bulk concentrated in `migrate/` (221) + `descriptor.rs` (78) + `live_migrate/` (55) |
| `djogi-macros` | 0 | Public surface fully documented |
| `djogi-cli` | 3 | `--format` enum variants only; `djogi-cli` is a binary, not adopter-API |
| `djogi-shell` | 0 | `lib.rs` has `#![allow(missing_docs)]` (verify whether this is intentional) |
| `djogi-maahi` | n/a | Crate not yet present in workspace |

## Findings

| Severity | Item | Location | Issue | Fix |
|---|---|---|---|---|
| block-publish | `pub trait Model` itself | `djogi/src/model.rs:66` | The single most adopter-facing trait in the framework has zero trait-level rustdoc — every adopter writes `impl Model for ...` (via `#[derive(Model)]`) and learns the trait by reading its associated-type list. | Add a trait-level doc block: what `Model` is, who implements it (the macro, not adopter code), the relationship to `__sealed::Sealed`, links to `docs/spec/models.md`. |
| block-publish | `pub mod prelude` | `djogi/src/lib.rs:371` | The prelude module — what every adopter is told to glob-import via `use djogi::prelude::*;` — has no module-level docstring explaining what is in scope or the prelude policy. | Add a module-level rustdoc block: stable contract, what is included vs deliberately excluded (e.g. raw bypass traits stay out), `#[doc(hidden)]` items inside the prelude. |
| block-publish | `pub enum DjogiError` itself | `djogi/src/error.rs:167` | The crate's top-level error enum has no enum-level rustdoc — only the variants are documented. Adopters who match on `DjogiError` see the variants but no overview of error classes, retry semantics, or `#[non_exhaustive]` contract. | Add an enum-level doc block summarising the error taxonomy (auth / db / not-found / decode / lock-conflict / validation / etc.), retry policy via `is_transient`/`is_lock_error`, and the `#[non_exhaustive]` rule. |
| fix-before-alpha | `pub struct DjogiConfig` and fields | `djogi/src/config.rs:13` (struct), `:14`, `:15`, `:46`, `:64`, `:69`, `:70` | Top-level config struct + `database`, `server`, `DatabaseConfig.url`, `dev_mode`, `ServerConfig.host`, `:port` fields are all undocumented. `DjogiConfig` is what every adopter calls `DjogiConfig::load()` on. | Add struct-level rustdoc plus per-field `///` for each undocumented `pub` field. (`migrate`, `policy`, `crud_log_url`, etc. are already documented — finish the rest.) |
| fix-before-alpha | `AuthContext` public fields | `djogi/src/auth/mod.rs:22`, `:23`, `:24`, `:25` | `pub user_id`, `pub tenant_id`, `pub scopes`, `pub ext` — all four documented at the struct level but the fields themselves carry no rustdoc. Adopters can construct `AuthContext` directly via struct literal, so per-field semantics matter. | Add `///` for each field describing its semantics and constraints. |
| fix-before-alpha | `pub enum FieldSqlType` variants | `djogi/src/descriptor.rs:82-115` | 14 of the variants (`Text`, `SmallInt`, `Integer`, `BigInt`, `Real`, `DoublePrecision`, `Boolean`, `Timestamptz`, `Date`, `Numeric`, `Uuid`, `Jsonb`, `TextArray`, `IntegerArray`, `BigIntArray`, `BoolArray`) are undocumented. The enum-level docstring gives general context but variants need at least a one-liner each (`Citext`, `Geography`, `Custom` are documented — the typed-Rust → SQL mapping should be just as visible for the common variants). | Add a one-line `///` per variant: e.g. `/// SQL TEXT — Rust String / &str`. |
| fix-before-alpha | `pub enum GeographySubtype` variants | `djogi/src/descriptor.rs:48-53` | 6 PostGIS subtype variants (Point / LineString / Polygon / MultiPoint / MultiLineString / MultiPolygon) — enum-level docstring covers context but the variants are bare. | One-line `///` per variant naming the PostGIS geometry type. |
| fix-before-alpha | `pub enum IndexType` variants | `djogi/src/descriptor.rs:184-189` | `BTree`, `Gist`, `Gin`, `Hash`, `Spgist`, `Brin` — undocumented. | One-line `///` per variant — when to reach for each (BTree default, GIN for arrays/tsvector/JSONB, GiST for spatial/range, BRIN for time-series). |
| fix-before-alpha | `pub enum IndexKind` variants | `djogi/src/descriptor.rs:212-214` | `NonUnique`, `UniqueConstraint`, `UniqueIndex` — enum-level docstring covers semantics in prose but variants are bare. | One-line `///` per variant referencing the enum-level table. |
| fix-before-alpha | Migration descriptor enums | `djogi/src/descriptor.rs` (78 warnings total) | 78 missing docs across `IndexOrder`, `IndexNullsOrder`, `IndexColumnSpec`, `IndexTarget`, `IndexSpec`, `PartitionSpec`, `PkType`, `RetentionLabel`, `RedactionPolicy`, `Sensitivity`, `ProtectedFieldMetadata`, `ModelDescriptor` — all re-exported from `prelude`. The variants/fields are partly documented; the gaps are uneven. | Per-field `///` pass against each `pub` struct field and enum variant; the bulk are one-liners that follow obvious patterns. |
| fix-before-alpha | `SchemaOperation` enum struct fields | `djogi/src/migrate/diff.rs` (43 warnings) | The enum *variants* are well-documented (`AddTable`, `DropTable`, `RenameTable`, etc.) but the *struct-style fields inside variants* (`from`, `to`, `column`, `change`, `fk`, `exclusion`, etc.) are bare. `SchemaOperation` is `pub use`-able from `migrate::diff::*` and underlies the differ contract Phase 7 ships. | Per-field `///` for each variant struct field. The enum-level docstring already explains the operation taxonomy. |
| fix-before-alpha | Migration runner / attune / repair / reset / projection / schema / seed / verify / compose / ledger / guard public types | `djogi/src/migrate/{runner,attune,repair,reset,schema,seed,verify,compose,ledger,guard,snapshot,docs,target,bootstrap,projection,pk_flip}.rs` (180 warnings) | Bulk of public-but-internal-feeling pubs in the migrate substrate. These types are accessible from `djogi::migrate::*` but are not re-exported through `prelude`. Adopters reaching for the migration engine programmatically (custom CI wrappers, KindNudge-style migration orchestration) hit them; most adopters use `djogi migrations` CLI and never see them. | Bulk task: either (a) gate behind `#[doc(hidden)]` for the truly-internal ones (the `pub` is for cross-module access, not adopter API), or (b) write per-item rustdoc. Recommend a triage pass before publish — every `pub` should be intentional. Tracking: file separate audit issue for each `migrate/` file. |
| fix-before-alpha | `live_migrate/` public types | `djogi/src/live_migrate/{plan,plan_file,state,patterns/mod,backfill}.rs` (55 warnings) | Same pattern as `migrate/` — public types underpinning the live-migration substrate (Phase 7.5). Largely descriptor-style enums + structs with field-by-field `///` gaps. | Same triage: doc-hide internal-only types; document the public-facing ones. |
| fix-before-alpha | `Q<T>` field accessors | `djogi/src/query/q.rs:252` | One method + struct-field combo missing — central to the predicate algebra. | Add `///` for the `Q<T>` member. |
| fix-before-alpha | `query/condition.rs` Condition variants | `djogi/src/query/condition.rs:523-539` (15 warnings) | Public condition tree underlying `QuerySet`. Variants undocumented despite being the canonical type for predicate composition (legacy escape per CLAUDE.md but still public via `query::internal::Condition`). | Per-variant `///` or move to `query::internal::*` namespace and `#[doc(hidden)]`. |
| fix-before-alpha | `field_codec`, `intent`, `tracked`, `notify` public items | `djogi/src/{field_codec,intent,tracked,notify}.rs` (5+4+4 warnings) | Smaller public surfaces with field-level gaps. `Tracked` is in the prelude. | Per-field `///` pass. |
| fix-before-alpha | `expr/window_fn.rs:647` window-function struct | `djogi/src/expr/window_fn.rs:647` | Window-function struct + 6 undocumented field references. The struct itself has the docstring missing too. | Add struct-level + per-field rustdoc. |
| fix-before-alpha | `snapshot/sign.rs:200`, `:206` | `djogi/src/snapshot/sign.rs` | 3 missing-docs warnings on the snapshot signing surface. The module-level + the two main `sign_snapshot` / `verify_snapshot` functions are well-documented; the gaps are on smaller helpers/types nearby. | Per-item rustdoc; respect the existing module-level threat-model framing. |
| nit | `pub use heeranjid::HeerId` re-exports | `djogi/src/lib.rs:365-368` | 7 re-exports of `HeerId` / `HeerIdDesc` / `RanjId` / `RanjIdDesc` / `Date` / `DateTime` / etc. through `crate::types`. These are third-party-style — documenting them by reference (link to upstream HeeRanjId rustdoc) is the right shape, not duplicating the upstream docstring. | One module-level note in `types.rs` pointing at HeeRanjId; let `pub use` carry through. The current rustdoc warning fires on the module-level `pub use` line; adding a brief `///` for the re-export tagline is enough. |
| nit | `djogi-cli` `--format` enum variants | `djogi-cli/src/main.rs:144`, `:165`, `:166` | `Json`, `Human`, `Json` variants of two different `--format` enums in the CLI. CLI binary is not adopter-API surface; clap handles the help text at runtime. | One-line `///` per variant for rustdoc completeness; not adopter-blocking. |
| info | `djogi-shell` is silently allowing missing-docs | `djogi-shell/src/lib.rs` (likely contains `#![allow(missing_docs)]`) | `cargo rustdoc -p djogi-shell --lib --all-features -- -W missing_docs` produced zero warnings, but the shell crate has substantial `pub fn` surface for Rhai bindings. | Verify the shell crate intends to suppress missing-docs or whether the lint is mis-scoped. If intentional (shell internals are not adopter API), add a one-line module-level explanation. Anchor: defer to Cluster 6 docs-sweep follow-up after sister Pass 2/3 outcomes are reconciled. |
| info | `djogi-macros` clean | n/a | Zero missing-docs warnings on the proc-macro crate. The four warnings in its rustdoc log are all unresolved-link warnings (`djogi::AppDescriptor` from inside the macro crate where `djogi` is not in scope). | Out of scope for Pass 1; track separately as a doc-link audit if needed. Anchor: Cluster 6 docs-sweep follow-up. |

## Coverage stats
- Items audited: 423 missing-docs warnings (420 djogi + 3 djogi-cli + 0 djogi-macros + 0 djogi-shell)
- Block-publish: 3 (Model trait, prelude module, DjogiError enum — top-level adopter-facing items with zero rustdoc)
- Fix-before-alpha: 13 categories spanning 410+ individual warnings (DjogiConfig, AuthContext fields, descriptor enum variants, migration substrate, live_migrate, query/condition, query/q, field_codec/intent/tracked/notify, expr/window_fn, snapshot/sign)
- Nits: 2 (HeerId re-exports, djogi-cli format enums)
- Info: 2 (djogi-shell lint suppression, djogi-macros doc-link warnings)

## Self-review caveat
careful-coder Opus output; not GPT-5.5 ALLOW.
