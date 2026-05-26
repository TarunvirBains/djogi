ledger_slug: durable-ledger-2026-05-26-issue-341-cli-unify

# Issue #341 — Unify CLI Migration Commands: Remove djogi migrate Stub

**Status:** COMPLETE — all REQ items verified, PR ready for review.
**Branch:** `beta-blocker/issue-341-cli-unify`
**Commits:** 2 implementation commits

## Requirements Ledger

| ID | Source | Requirement | Status | Evidence |
|---|---|---|---|---|
| REQ-341-1 | Issue #341 L1 | Remove `TopCommand::Migrate` enum variant from main.rs:34 | COMPLETE | Commit 29b99874, variant removed |
| REQ-341-2 | Issue #341 L1 | Remove match arm for `TopCommand::Migrate` from main.rs:431-434 | COMPLETE | Commit 29b99874, match arm removed |
| REQ-341-3 | Issue #341 L2 | Update guard.rs:10 doc comment — "djogi migrate" → "the migration tooling" | COMPLETE | Commit d3f28d82 |
| REQ-341-4 | Issue #341 L3 | Update apps-and-database-domains.md:98 — "djogi migrate startup" → "migration startup" | COMPLETE | Commit d3f28d82 |
| REQ-341-5 | Issue #341 L3 | Update orm-gap-analysis.md:407,415,517 — `djogi migrate` → `djogi migrations` | COMPLETE | Commit d3f28d82 |
| REQ-341-6 | Issue #341 L4 | Update GitHub issues #325-327, #340 to canonical spelling | COMPLETE | Verified via gh issue view — zero actionable OLD matches |
| REQ-341-7 | Codex F2 | Update issue #341 acceptance wording for historical exceptions | COMPLETE | Acceptance criterion now permits historical design-history references |
| REQ-341-8 | First-pass non-goals | Do not modify historical docs, research/, superpowers/, migration-proposal.md:232-233 | COMPLETE | git diff shows only 4 allowed files changed |
| REQ-341-9 | First-pass scope | Edit only listed files; no macro/runner/verify/compose changes | COMPLETE | Diff: main.rs, guard.rs, apps-and-database-domains.md, orm-gap-analysis.md |
| REQ-341-10 | Codex F3/F5 | Add CLI parser regression tests: migrate rejected, migrations still parses | COMPLETE | Tests `top_level_migrate_is_not_registered` + `canonical_migrations_status_still_parses` both PASS |
| REQ-341-11 | Codex F3 | Document intentional behavior change: old stub → fast-fail unknown command | COMPLETE | Test names and behavior pinned; plan notes intentional change |

## CI Gate Results

| Gate | Status | Evidence |
|---|---|---|
| cargo build --all-features | PASS | 19 crates compiled, clean |
| cargo test --workspace | PASS | 3755 passed, 239 ignored |
| Lihaaf: djogi-macros | PASS | 336 OK |
| Lihaaf: djogi raw-SQL | PASS | 49 OK |
| clippy --all-targets --all-features -D warnings | PASS | No issues |
| fmt --all --check | PASS | Clean |
| check-secrets --staged | PASS | No findings |
| Parser regression tests | PASS | Both tests pass |

## Diff Summary

```
djogi-cli/src/main.rs                  | 30 +++++++++++++++++++++++-------
djogi/src/migrate/guard.rs             |  2 +-
docs/spec/apps-and-database-domains.md |  2 +-
docs/spec/orm-gap-analysis.md          |  6 +++---
4 files changed, 28 insertions(+), 12 deletions(-)
```

## Notes

- Behavior change: `djogi migrate` was never functional (printed stub + exit 0); now fails fast as unknown command. This is intentional and safer.
- Issues #325-327, #340 updated to canonical `djogi migrations ...` spelling.
- Issue #341 acceptance wording updated to allow historical design-history references.
