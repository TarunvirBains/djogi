# Contributing to Djogi

Thanks for your interest in djogi. This document covers the
practical bits — how to set up, run tests, and propose changes.

## Setup

Djogi targets **Postgres 18 and later, exclusively** (see
`docs/spec/decisions.md` for the rationale). You will need:

- Rust 1.85 or later (edition 2024)
- A local Postgres 18+ instance, with the `postgis` extension
  installed if you intend to run spatial tests
- `cargo` and the standard toolchain

Clone the repo, run the setup, run the tests:

```bash
git clone https://github.com/TarunvirBains/djogi
cd djogi
cargo build
DATABASE_URL=postgres://djogi:djogi@localhost:5432/djogi_test \
  cargo test --workspace -- --test-threads=1
```

Integration tests assume a running Postgres at `DATABASE_URL` with
permission to `CREATE EXTENSION` and `CREATE ROLE`. For most
day-to-day development, lib tests are sufficient and don't need a
database:

```bash
cargo test --lib --workspace
```

## Code style

- Follow stdlib conventions; idiomatic Rust over clever
- `cargo fmt --all` before commit
- `cargo clippy --all-targets --all-features -- -D warnings` must
  pass
- No `regex` engine, no regex notation in comments or messages —
  the project uses byte-level checks and explicit rules instead
  (see `docs/spec/decisions.md`)
- Atomic commits: each commit one logical unit, passes tests in
  isolation

## Secrets hygiene

Before every commit, and before pasting issue / PR text into public
GitHub, run the secret-pattern scanner:

```bash
cargo xtask check-secrets --staged          # pre-commit
cargo xtask check-secrets --stdin < draft.md # pre-issue / pre-PR-body
cargo xtask check-secrets                    # full repo sweep
```
Public GitHub bodies and comments also go through
`.github/workflows/public-text-secrets.yml` so repository guards cover
`issues`, `issue_comment`, `pull_request`, `pull_request_review`,
and `pull_request_review_comment` text without exposing raw secrets in CI logs.

The scanner reports any URL with embedded `user:password`, any known
secret env-var assignment, and any PEM private-key block header. Output
is redacted (the raw value is never echoed). Intentional fixtures and
anti-pattern examples can be allowlisted with a
`// djogi-allow-secret: <reason>` marker — see
[`docs/guide/secrets-hygiene.md`](docs/guide/secrets-hygiene.md) for
the full allowlist mechanism and a list of recognised placeholder
forms.

## Proposing changes

1. **Open an issue first** for non-trivial changes. The
   [implementation plan](docs/spec/implementation-plan.md)
   sequences the framework's build; please align with it where
   relevant.
2. **Fork and branch.** Branch off `main`, name it descriptively
   (`feature/foo`, `fix/bar`).
3. **Tests are required** for new behavior. Add at least one
   integration test that hits a live Postgres if the change
   touches the data layer.
4. **Document public surface.** Public functions, types, and
   traits get rustdoc with what / why / how / where, in line
   with the existing style. Adopters read these via docs.rs.
5. **Open the PR.** Reference the issue, describe the change in
   2–4 bullets, list a test plan. The PR template prompts for
   these.

## What's in scope

Djogi is a **Model-first framework** — it owns the data layer
derivation chain (ORM, migrations, descriptors, audit trail,
shell bindings). It explicitly does not wrap or compete with
HTTP frameworks; integrations with Axum / Warp / Actix / etc.
ship behind opt-in feature flags. See `ReadMe.MD` and
`docs/spec/scope.md` for the boundaries.

## Pre-publish framing

Until v0.1.0 is published to crates.io, djogi has no external
users — internal APIs are reshaped freely without deprecation
paths or compat shims. After v0.1.0, breaking changes follow
semver discipline.
