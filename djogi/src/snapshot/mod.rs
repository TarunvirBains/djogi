//! Migration snapshot integrity primitives.
//! `migrations/schema_snapshot.json` records the schema-of-record after every
//! successful `djogi migrations apply`. The runner persists it atomically
//! the descriptor differ then reads it back on the next `cargo build` to
//! decide whether to emit a new pending migration. If a snapshot file is
//! tampered with (e.g. someone hand-edits it to suppress a drift warning,
//! or a corrupted CI cache resurfaces a stale copy), the differ will silently
//! mis-classify drift and the migration history goes out of sync with the
//! live schema.
//! The `snapshot::sign` submodule (this module's first inhabitant) provides
//! the HMAC-SHA256 sign-and-verify primitives that detect such tampering.
//! Future tasks in add the higher-level surfaces:
//! - **T9.4 / T9.5** — `djogi_ddl_audit` table and the `record_ddl`
//!   wiring inside `apply_plan_inner`. Every applied DDL plan writes a
//!   tamper-evident audit row that pairs with the snapshot signature
//!   stored alongside `schema_snapshot.json`.
//! - **T9.6** — the `djogi verify` CLI subcommand that reads the on-disk
//!   snapshot, recomputes its HMAC-SHA256 signature, and compares it
//!   constant-time against the value the runner persisted.
//! # Key model
//! Signing is **opt-in** via the `DJOGI_SNAPSHOT_SIGNING_KEY` environment
//! variable (64 hex characters, decoding to a 32-byte HMAC key). Without
//! the env var set, the runner uses a sentinel `[0u8; 32]` "no-op key" and
//! emits zero-byte signatures — both `sign_snapshot` and `verify_snapshot`
//! recognise the sentinel and round-trip cleanly. This keeps dev/CI runs
//! signing-free while letting production deployments enforce snapshot
//! integrity by setting the env var.
//! See `docs/spec/v3-cluster-8e-plan.md` lines 169–173 (D8) and 450–451 for
//! the full design rationale.

pub mod sign;
