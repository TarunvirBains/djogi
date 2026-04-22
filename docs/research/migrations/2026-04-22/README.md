# Migration Systems Research — Source-Backed Proposal

**Date:** 2026-04-22
**Status:** In progress
**Purpose:** An exhaustive, source-backed alternative proposal for Djogi's migration and live-migration design, intended to become the canonical reference for locking final decisions.

---

## Why this document exists

Djogi already has:

- `docs/spec/migrations.md` — the canonical 0.1.0 migration spec
- `docs/spec/decisions.md` — locked cross-cutting decisions
- `docs/superpowers/specs/2026-04-22-phase7-migration-system-design.md` — Phase 7 architectural design
- `docs/superpowers/plans/2026-04-18-phase7-migration-system-v2.md` — current implementation plan
- `docs/superpowers/research/2026-04-20-migration-runner-matrix.md` — the existing comparison matrix (doc-level, not source-level)

**What this document adds:** the existing matrix is built from framework documentation and blog posts. This proposal is built from reading the actual source code of 11 mature migration systems, which are cloned locally as reference repos inside the Djogi workspace. Every claim in this document is backed by a file path and (where applicable) a line number from one of those clones.

**This document does not argue with locked decisions** unless source-level evidence contradicts the reasoning behind the lock. Where it does, that disagreement is explicit and surfaced in `13-gap-analysis-vs-current-spec.md`.

---

## Source repos (clones)

All clones are at `/home/tarunvir/projects/<name>-reference/` and symlinked from `/home/tarunvir/projects/djogi/<name>-reference`.

| System | Language | Repo | Clone path |
|---|---|---|---|
| Django | Python | django/django | `django-reference/` |
| Alembic | Python | sqlalchemy/alembic | `alembic-reference/` |
| SQLAlchemy | Python | sqlalchemy/sqlalchemy | `sqlalchemy-reference/` |
| Flyway | Java | flyway/flyway | `flyway-reference/` |
| Liquibase | Java | liquibase/liquibase | `liquibase-reference/` |
| Prisma | Rust/TS | prisma/prisma | `prisma-reference/` |
| Diesel | Rust | diesel-rs/diesel | `diesel-reference/` |
| SeaORM | Rust | SeaQL/sea-orm | `sea-orm-reference/` |
| SeaQuery | Rust | SeaQL/sea-query | `seaquery-reference/` (symlinked as `sea-query-reference`) |
| refinery | Rust | rust-db/refinery | `refinery-reference/` |
| cot | Rust | cot-rs/cot | `cot-reference/` |

---

## Structure

```
docs/research/migrations/2026-04-22/
├── README.md                                       # This file
├── projects/                                       # Per-project source-read notes
│   ├── django.md
│   ├── alembic.md
│   ├── sqlalchemy.md
│   ├── flyway.md
│   ├── liquibase.md
│   ├── prisma.md
│   ├── diesel.md
│   ├── sea-orm.md
│   ├── sea-query.md
│   ├── refinery.md
│   └── cot.md
├── topics/                                         # Cross-cutting syntheses (written after projects/)
│   ├── 01-source-of-truth-and-state.md
│   ├── 02-ledger-schema.md
│   ├── 03-checksums-and-repair.md
│   ├── 04-advisory-locks-and-concurrency.md
│   ├── 05-transactional-vs-non-transactional.md
│   ├── 06-out-of-order-and-baseline.md
│   ├── 07-rename-handling.md
│   ├── 08-composite-uniques-and-indexes.md
│   ├── 09-destructive-and-lossy-classification.md
│   ├── 10-online-safe-staged-migrations.md
│   ├── 11-diff-algorithms.md
│   └── 12-rust-ecosystem-contrast.md
├── 13-gap-analysis-vs-current-spec.md              # Maps findings against existing Djogi docs
├── 14-locked-recommendations.md                    # Actionable proposal: adopt/reject/defer
└── 15-synthesis-and-dissent.md                     # Convergence, divergence, open questions
```

Reading order for a reviewer with limited time:

1. `README.md` (this file)
2. `14-locked-recommendations.md` (the actual proposal)
3. `15-synthesis-and-dissent.md` (where the 11 systems disagree and why)
4. `13-gap-analysis-vs-current-spec.md` (what changes vs current spec)
5. Dive into `topics/` for any disputed area
6. Dive into `projects/` for source-level verification of any specific claim

---

## Research methodology

### Hard rules

1. **Every claim cites source.** Format: `path/inside/clone.py:LINE` or `path:LINE-LINE` for ranges. No uncited assertions.
2. **Quote DDL verbatim.** For ledger tables, lock tables, and checksum formats, paraphrasing loses precision.
3. **Label confidence.** Three levels:
   - `high` — read the source code
   - `medium` — read the test suite and inferred behaviour
   - `low` — docs only, no source verification
4. **No re-arguing locks silently.** If a finding contradicts a locked decision, surface it in `13-gap-analysis-vs-current-spec.md` with the evidence, then defer to the user.
5. **Supersede-don't-delete the existing matrix.** The 2026-04-20 matrix is useful as a quick-lookup; this document links to it rather than replacing it.

### Per-project file template

Each `projects/*.md` file has the following sections:

```markdown
# <System>

## Metadata
- Clone path, commit SHA inspected, primary language, total LOC of migration-relevant modules

## Architecture
- Where migration code lives (paths)
- Key files and their roles

## State model (source-of-truth)
- Descriptors / models / files — which is canonical
- What's tracked in the database vs the filesystem vs memory
- Separation of applied-state from execution-history

## Ledger / history table
- Exact DDL (quoted), column purposes, indexes, primary key strategy
- Path where DDL is defined

## Execution
- Lock strategy (advisory lock / lock table / none)
- Transaction boundaries (default; opt-out; auto-detect)
- How non-transactional DDL is handled
- Concurrency posture

## Recovery
- Checksum algorithm (what's hashed, how)
- Repair commands (what they mutate, what they refuse)
- Baseline / stamp / fake flows
- Partial-apply handling
- Out-of-order policy

## Diff and generation
- Autogen algorithm (how desired-vs-applied is computed)
- Rename handling (heuristic / explicit / none)
- Destructive-operation detection and gating

## Schema metadata
- Composite unique constraints: representation, naming
- Composite indexes: representation, naming
- Reflection / introspection capability

## Online-safe / staged migration guidance
- Any built-in support? Any documented patterns? Any warnings?

## Rust-specific concerns (Rust repos only)
- Async model, type-safety surface, macro use, proc-macro use

## Lessons for Djogi
- **Adopt:** ... (with source citation and rationale)
- **Reject:** ... (with source citation and rationale)
- **Defer:** ... (with criteria for revisit)
- **Surprises:** things that contradict or extend the current Djogi spec
```

---

## Execution plan (batches)

Per-project research is done in batches of 3 to keep context manageable. Between batches, context is compacted / cleared so the main conversation remains clean.

| Batch | Systems | Status |
|---|---|---|
| 1 | django, alembic, sqlalchemy | pending |
| 2 | flyway, liquibase, prisma | pending |
| 3 | diesel, sea-orm, sea-query | pending |
| 4 | refinery, cot | pending |
| Synthesis | topics/ 01-12 | pending |
| Gap analysis | 13-gap-analysis-vs-current-spec.md | pending |
| Recommendations | 14-locked-recommendations.md | pending |
| Synthesis & dissent | 15-synthesis-and-dissent.md | pending |

Each batch dispatches N parallel subagents, each writing directly to `projects/<name>.md`. After each batch, citations are spot-checked before proceeding.

---

## Scope guardrails

**In scope:**

- Migration runners, ledger/history storage, locking, recovery
- Diff / autogen algorithms
- Naming and reflection for constraints and indexes
- Transactional vs non-transactional execution
- Online-safe / staged migration patterns
- Rust-specific integration patterns where relevant

**Out of scope:**

- General ORM query-builder design
- Web framework integration
- Performance benchmarks unrelated to migration execution
- Tutorials / example apps

---

## Positioning against existing Djogi docs

This document **does not replace** any existing spec. It is a research alternative proposal intended to either:

- Reinforce existing decisions with source-level evidence (default case)
- Surface underspecified areas for follow-up in the Phase 7 plan
- Flag source-level evidence that contradicts a locked decision (rare, but must be explicit)

The final output — `14-locked-recommendations.md` — is written as a reviewable proposal, not as an automatic override of existing specs.
