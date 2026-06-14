# Drift Detection

Djogi records a schema snapshot per migration bucket at:

`migrations/<database>/<app>/schema_snapshot.json`

That snapshot is the schema-of-record for the bucket. Drift detection compares
that recorded baseline with the live PostgreSQL catalog and reports any
structural mismatch before operators keep composing, applying, or repairing
history around an unknown database shape.

This guide covers the verification primitives Djogi ships today, how adopters
should integrate them, and what the current system intentionally does not try
to prove.

## What "drift" means

Drift is any difference between:

- the recorded bucket snapshot and ledger expectations
- the live PostgreSQL catalog for that same bucket

Examples:

- a table or column was added manually in production
- an index was dropped out of band
- a foreign key shape differs from what the committed migration history says
- `schema_snapshot.json` was deleted, corrupted, or no longer matches the live DB

Djogi surfaces drift through D6xx diagnostics. Error-severity diagnostics block
default-on apply-time verification; warnings and infos remain advisory.

## The verification primitives

Djogi exposes three practical verification surfaces:

- `djogi migrations verify`
- `djogi::migrate::verify`
- `djogi::migrate::verify_bucket`

`djogi migrations verify` is the normal operator entry point. It loads the
recorded snapshot for each discovered bucket, projects the live catalog, and
renders a deterministic report.

`verify_bucket` is the narrow primitive the runner now reuses during apply. It
checks one bucket against one snapshot on the same database session that is
about to apply migrations.

## Default-on apply-time gate

`djogi migrations apply` now runs a bucket-scoped drift pre-flight before any
migration SQL executes.

The gate is default-on for real apply:

- if the bucket has never been applied, the gate self-skips (drift is undefined without a prior applied state)
- if the bucket has applied history and the snapshot is missing, apply refuses
- if the bucket has applied history and the snapshot is corrupt (present but unreadable), apply refuses
- if the bucket has applied history and verify finds error-severity drift, apply refuses
- if verify itself cannot complete, apply fails before user-schema mutation

`djogi migrations apply --fake` is intentionally exempt:

- it does not run the pre-flight
- it does not read `schema_snapshot.json`
- a missing or corrupt snapshot must not block existing-database adoption

Refusal output reuses the same rendered verification report as
`djogi migrations verify`, followed by concrete next steps.

## Recommended integration patterns

### 1. Default production apply

Use plain `djogi migrations apply` and let the built-in drift gate protect the
database from applying new SQL on top of an already-diverged catalog.

This should be the baseline posture for real environments. Do not disable the
gate in ordinary CLI usage.

### 2. Pre-deploy CI gate

Run `djogi migrations verify` against a database that reflects the deployment
target before rollout. A read-only production replica is ideal when available;
otherwise use a fresh environment seeded from the same migration state.

This gives you the full D6xx report earlier in the pipeline, while the apply
gate remains the final safety check on the actual apply session.

### 3. Periodic production monitoring

Schedule `djogi migrations verify` as an operational check against long-lived
databases. This catches out-of-band DDL even when no deploy is in flight.

Treat new error-level diagnostics as incidents. Warning-only output can be
triaged during normal maintenance windows.

### 4. Local development habit

Run `djogi migrations verify` when:

- manual psql experimentation happened in the dev database
- a branch was rebased or migration history was rewritten
- `schema_snapshot.json` was resolved through a merge

This keeps drift visible before it turns into confusing compose/apply behavior.

### 5. Compose-and-apply CI loops

For workflows that `compose` and then immediately `apply`, keep both stages:

- `compose` produces the pending migration and next snapshot
- `apply` verifies the previously-recorded snapshot against live state first

These are different checks. Compose answers "what should change next"; the
apply-time gate answers "is the database still where the committed history says
it is right now?"

## Triage and repair

When drift is intentional, reconcile it explicitly instead of bypassing the
gate silently.

Primary tools:

- `djogi migrations attune`
- `djogi migrations repair snapshot-rebuild`
- `djogi migrations baseline`

General guidance:

- use `attune` when migration history and on-disk artifacts need reconciliation
- use `repair snapshot-rebuild` when the recorded snapshot is missing or stale
- use `baseline` when adopting an existing schema as the new starting point

If the gate refuses on missing snapshot for an already-applied bucket, that is
not a harmless skip condition. The snapshot is part of the migration contract
and must be restored or rebuilt.

## Reading D6xx output

D6xx diagnostics are the drift vocabulary.

Common operator stance:

- `ERROR`: do not apply new migrations until reconciled
- `WARN`: investigate, but the runner does not block on warnings alone
- `INFO`: visibility only

The current registry lives in the verify implementation. The full set of
diagnostics and their severities:

| Code | Diagnostic | Severity |
|------|-----------|----------|
| D601 | Snapshot table missing in live catalog | Error |
| D602 | Live table not in snapshot | Error |
| D603 | Snapshot column missing in live | Error |
| D604 | Live column not in snapshot | Error |
| D605 | Column nullability drift | Error |
| D606 | Column type-string drift | Warning |
| D607 | Column default drift | Error |
| D608 | Primary-key column list drift | Error |
| D609 | Foreign-key shape drift | Error |
| D610 | Snapshot index missing in live | Error |
| D611 | Extra live index not in snapshot | Warning |
| D612 | Index column list drift | Error |
| D613 | Index uniqueness drift | Error |
| D614 | Index access method drift | Warning |
| D615 | Index attached to wrong table | Error |
| D621 | Ledger table not found *(suppressed at apply-time)* | Error |
| D622 | Out-of-order migration detected *(suppressed at apply-time)* | Warning/Error |
| D623 | Repair refused: leaf identity mismatch | Error |
| D624 | Rollback refused: leaf identity mismatch | Error |
| D690–D693 | Feature not yet verified (FTS, partition, enums, partial indexes) | Info |
| D699 | Ledger reports applied but DB has no tables *(suppressed at apply-time)* | Error |

D606, D611, and D614 are advisory (Warning) because the divergence may be a
legitimate operator choice — a manually-tuned index method, an extra
covering index, or a type rendering that differs only cosmetically. D622 is a
Warning by default and upgrades to Error under strict out-of-order policy.
Everything else is an Error: the apply-time gate refuses only when at least
one Error-severity diagnostic is present, so a Warning- or Info-only report
still allows apply.

The apply-time gate suppresses D621, D622, and D699 (ledger-lifecycle
diagnostics); the runner bootstraps the ledger before the gate and owns its
own out-of-order preflight.

## Known boundaries and non-goals

Current system boundaries:

- drift detection is schema-focused; it does not validate application runtime queries
- cross-database distributed correctness is still an orchestration concern
- a bucket with no applied history self-skips the apply-time gate by design
- `--fake` remains an explicit adoption tool, not a verification path

Not part of this guide's scope:

- deferred runtime-query verification work tracked separately in `#153`
- proving application-level semantic equivalence after manual DDL
- replacing normal review of destructive migrations

## Suggested operating model

Use the tools in this order:

1. `djogi migrations verify` for broad visibility
2. `djogi migrations attune` or `repair snapshot-rebuild` when state must be reconciled
3. `djogi migrations apply` for the final, session-local pre-flight and mutation

That keeps drift detection explicit, repeatable, and close to the actual schema
contract Djogi is enforcing.
