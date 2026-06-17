# Phase 9 CLI / Admin / Shell — Reconciliation Required Against #417

**Status:** Phase 9 CLI/admin/shell planning is downstream of issue #417
(CLI package surface unification) and must be reconciled against the #417
decision BEFORE Phase 9 implementation begins.

## What #417 settled

- The CLI ships from a single published package: **`djogi-cli`**, owning
  both executables `djogi` (canonical operator/runtime command) and
  `cargo-djogi` (developer `cargo djogi` wrapper).
- `djogi <subcommand>` is the canonical command language. `cargo djogi
  <subcommand>` is a local-development convenience wrapper only.
- Production/CI run the **prebuilt adopter-linked `djogi` binary**
  directly, never `cargo djogi`.

## Phase 9 items that must be reconciled before implementation

- Any Phase 9 plan that hard-codes a separate `cargo-djogi` package, or
  treats `cargo djogi` as the primary operator entrypoint, must be updated
  to the #417 model (canonical `djogi`; `cargo djogi` = dev wrapper from
  `djogi-cli`).
- The ownership/naming of bare commands raised in Phase 9 discussion
  (`djogi check`, `djogi stats`, `djogi prepare`, `djogi check-docs`) and
  the spec-named forms (`djogi analyze query`, `djogi admin set-password`,
  `djogi djqry verify`, `djogi db seed`) is a Phase 9 design question. #417
  does not decide command inventory — only the package/executable surface.
  Resolve command inventory during Phase 9 planning, on the canonical
  `djogi <subcommand>` surface.
- Plans should not bake crate/package structure into the adopter-facing
  contract; the contract is `cargo install djogi-cli` + the `djogi`
  command.

## Non-goal

This note does not rewrite the Phase 9 roadmap. The reconciliation pass is
separate Phase 9 planning work.
