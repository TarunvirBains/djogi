# Task 9 Ledger — Adopter Fixture Workspace + Integration Tests

**Issue:** djogi#370 (Adopter-linked djogi CLI: Descriptor Provider Boundary)
**Source spec:** `docs/superpowers/private-specs/issue-370/spec.md` (404 lines)
**Source plan:** `docs/superpowers/private-specs/issue-370/plan.md` (3218 lines, Task 9 = lines 2025-2565)

---

## Fixture Workspace — adopter_app (forced: all crates linked)

| ID | Source | Description | Verification Contract |
|----|--------|-------------|----------------------|
| T9-FIXTURE-WORKSPACE | Plan L2041-2050 | Virtual workspace at `djogi-cli/tests/fixtures/adopter_app/Cargo.toml` with `members = ["tracker", "billing", "bin"]`, resolver "2", own `[workspace]` isolated from outer djogi workspace. | `cargo check --manifest-path <fixture>/Cargo.toml` succeeds independently |
| T9-FIXTURE-TRACKER | Plan L2052-2086 | `tracker/` lib crate with TWO models: `Elephant { name: String }` (table "elephants") and `Herd { region: String }` (table "herds"). Both `#[derive(Model)]`. Dep: `djogi = { path = "../../../../../djogi" }` (5 levels up from member). | Both structs compile with Model derive; no `#[model(app = …)]` — global bucket |
| T9-FIXTURE-BILLING | Plan L2088-2102 | `billing/` lib crate with ONE model: `Invoice { reference: String }` (table "invoices"). `#[derive(Model)]`. Same djogi path-dep depth. | Compiles; this is the dead-strippable crate in unforced variant |
| T9-FIXTURE-BIN | Plan L2104-2137 | Binary crate `adopter-app-bin` with `[[bin]] name = "djogi"`. Deps: djogi, djogi-cli, djogi-macros, tracker (path), billing (path). Uses `djogi_cli::djogi_main!(tracker::Elephant, tracker::Herd, billing::Invoice)` — forces BOTH model crates. | Binary named `djogi` builds; lists at least one type per crate |
| T9-FIXTURE-LOCK | Plan L2141 | Committed `Cargo.lock` at workspace root + `.gitignore` with `target/`. | Lock file exists and is committed; generated via `cargo generate-lockfile` |

## Fixture Workspace — adopter_app_unforced (partial-miss: billing dead-strippable)

| ID | Source | Description | Verification Contract |
|----|--------|-------------|----------------------|
| T9-FIXTURE-UNFORCED | Plan L2143-2160 | Same three-crate workspace at `djogi-cli/tests/fixtures/adopter_app_unforced/` (copies, not path-deps — plan prefers copies for isolated `[workspace]` + lock determinism). EXCEPT `bin/src/bin/djogi.rs` references ONLY `tracker::Elephant` via hand-written forcing: `let _ = <tracker::Elephant as djogi::model::Model>::descriptor(); djogi_cli::run_from_env()`. `billing` is a dependency but NEVER referenced → linker dead-strippable. | Unforced binary builds; `schema` output contains elephants/herds but NOT invoices |
| T9-FIXTURE-UNFORCED-BIN | Plan L2147-2158 | Hand-written main (NOT `djogi_main!`) — hand-written so billing is genuinely unforced. Still lists billing as dep (available to link, just not referenced). | No reference to `billing::Invoice` anywhere in bin source |

## Integration Tests — adopter_linked_cli.rs

| ID | Source | Description | Verification Contract |
|----|--------|-------------|----------------------|
| T9-T-POS | Plan L2162-2259, Spec §10 T-POS row | `t_pos_adopter_binary_discovers_all_models`: build forced fixture, run `schema --format json` — assert elephants/herds/invoices all present. Also run `migrations compose` — assert pending artifacts exist and composed SQL contains all three tables. | Schema JSON + composed SQL contain elephants, herds, invoices |
| T9-T-NAM | Plan L2268-2281, Spec §10 T-NAME row | `t_name_binary_is_named_djogi_and_surface_matches`: assert binary file name is "djogi", run `migrations --help` — surface contains compose. | Binary named `djogi`; help output contains "compose" |
| T9-T-LINK | Plan L2283-2307, Spec §10 T-LINK row | `t_link_unforced_crate_is_invisible_forced_is_visible`: build both fixtures. Forced schema has invoices; unforced schema has elephants but NOT invoices. Cross-crate assertion is load-bearing. | Forced: contains invoices. Unforced: contains elephants, does NOT contain invoices |
| T9-T-DROPGUARD | Plan L2309-2338, Spec §10 T-DROPGUARD row | `t_dropguard_unlinked_app_refuses_even_with_allow_destructive`: seed a snapshot with billing app having one table + registered_apps. Run unforced binary compose with `--allow-destructive` — assert exit 2, linkage hint in stderr ("no models for it are linked now"), zero DROP migrations emitted. | Exit code 2; stderr contains linkage hint; no new migration files beyond seed |
| T9-T-PARITY | Plan L2340-2385, Spec §10 T-PARITY row | `t_parity_schema_and_compose_see_same_models`: run schema AND compose from forced fixture. Schema JSON contains all three tables; composed SQL contains all three tables. Proves one provider path. | Schema and compose both see elephants, herds, invoices |
| T9-T-VERIFY-DEGRADE | Plan L2387-2435, Spec §10 T-VERIFY-DEGRADE row | `t_verify_degrades_to_snapshot_only`: empty provider + on-disk snapshots present → verify enumerates from disk and verifies against snapshots (does NOT emit §5.6 refusal). Uses `#[djogi_test]` for Postgres. | Verify succeeds with snapshot-only; does not exit 2 when snapshots exist |
| T9-T-NOLOGIC | Plan L2437-2480, Spec §10 T-NOLOGIC row | `t_nologic_fixture_src_contains_only_models_and_glue`: verify fixture source contains only model definitions + glue. No duplicated compose/apply logic, no hand-maintained descriptor lists. Static analysis of fixture source files. | Fixture src/ contains only derive(Model) structs and djogi_main! call |
| T9-T-NEG | Plan L2482-2565, Spec §10 T-NEG row | Standalone negative: `compose`/`schema`/`docs` with zero descriptors each exit 2 with dual-cause diagnostic. Also rewrite `db.rs:927 docs_cmd_against_empty_inventory_succeeds` (was exit 0 → exit 2). | compose/schema/docs each exit 2; rewritten docs-empty test asserts exit 2 |

## Test Infrastructure

| ID | Source | Description | Verification Contract |
|----|--------|-------------|----------------------|
| T9-INFRA-DRIVER | Plan L2164-2226, Spec §10 header | `djogi-cli/tests/internal/adopter_linked_cli.rs` — single driver file. Helper functions: `cli_crate_dir()`, `workspace_root()`, `build_fixture_djogi(fixture, target_subdir)`, `tempdir_with_djogi_toml()`, `run_schema_json()`, `read_all_composed_up_sql()`. | All helpers defined once; reused across tests |
| T9-INFRA-REGISTRATION | Plan L2038-2039, djogi-cli/Cargo.toml pattern | Add `[[test]]` entry in `djogi-cli/Cargo.toml`: name = "adopter_linked_cli", path = "tests/internal/adopter_linked_cli.rs". Follow existing pattern from phase8 tests. | `cargo test -p djogi-cli --test adopter_linked_cli` discovers tests |
| T9-INFRA-CLI-HELPERS | Plan L2263, djogi/src/testing/cli.rs | Reuse `djogi::testing::cli::write_minimal_djogi_toml()` (already available via dev-dep with testing feature). Copy `test_database_url`/`splice_db_into_url` from phase8_verify_cli for DB-backed tests. | Helpers compile and work under #[djogi_test] context |
T9-INFRA-BUILD-Helpers | Plan L2192-2226 | `build_fixture_djogi()` builds fixture binary in release profile with `--locked`, dedicated `--target-dir`, `CARGO_TARGET_DIR` cleared. Returns path to compiled binary. | Builds succeed; binary path is correct |

## Verification Gates

| ID | Source | Description | Verification Contract |
|----|--------|-------------|----------------------|
| T9-VERIFY-BUILD | Plan L3100-3210, Spec §8 REQ-370-15 | Both fixture workspaces (`adopter_app`, `adopter_app_unforced`) build successfully in release. | `cargo build --release` for both fixtures succeeds |
| T9-VERIFY-TESTS | Plan L3100-3210, Spec §10 test table | All T-* tests pass: `cargo test -p djogi-cli --test adopter_linked_cli`. | Zero test failures |
| T9-VERIFY-CLIPPY | Spec §8 REQ-370-15 | `cargo clippy --all-targets --all-features` clean on djogi workspace. Fixture crates are `publish = false` and may use `#![allow(clippy::all)]` if needed. | Clippy passes for djogi workspace |

---

## Critical Design Decisions (from spec, must not be overridden)

1. **Fixtures are SEPARATE library crates, not modules** — Plan L2029 (codex BLOCK 10). Modules cannot reproduce crate-level dead-stripping.
2. **Two fixture workspaces: forced + unforced** — forced links both model crates; unforced only links tracker. Billing is dead-strippable in unforced.
3. **Unforced uses hand-written main, NOT djogi_main!** — Plan L2152. The macro forces everything; hand-written allows genuine partial-miss.
4. **Copies, not cross-workspace path-deps** — Plan L2137. Prefer copies for lock determinism in isolated workspace.
5. **Path depth: 5 levels up** — Plan L2065. `../../../../../djogi` from member crate to workspace root djogi/.
6. **No `#[model(app = …)]` in fixture models** — all project to global bucket, matching spec's synthetic global bucket behavior.
7. **T-DROPGUARD asserts exit 2 with --allow-destructive** — Plan L2309-2310 (the non-tautological case; default refusal is generic gate's job).
8. **T-NEG rewrites docs-empty test** — Spec §5.6: docs_cmd_against_empty_inventory_succeeds must change from exit 0 to exit 2.

## Previous Tasks Completed (Tasks 0-8, committed on feature branch)

- Task 0: Linker spike — one type per crate forces ALL inventory
- Task 1: DescriptorProvider trait in djogi library
- Task 2: InventoryDescriptorProvider implementation
- Task 3: pub projection/docs entrypoints (project_from_provider, generate_docs_with_provider)
- Task 4: Compose linkage-aware drop guard (REQ-370-16)
- Task 5: CLI lib split — run_from_env, run_with_args, run_with_provider
- Task 6: Rewire compose/verify/schema/docs to use provider
- Task 7: Rewire db.rs docs_cmd + standalone zero-descriptor diagnostic
- Task 8: djogi_main! variadic macro + link_anchor! fallback (with re-export fix)

## Implementation File Checklist

Create these files:
- [ ] `djogi-cli/tests/fixtures/adopter_app/Cargo.toml` (virtual workspace root)
- [ ] `djogi-cli/tests/fixtures/adopter_app/.gitignore` (`target/`)
- [ ] `djogi-cli/tests/fixtures/adopter_app/tracker/Cargo.toml`
- [ ] `djogi-cli/tests/fixtures/adopter_app/tracker/src/lib.rs` (Elephant + Herd)
- [ ] `djogi-cli/tests/fixtures/adopter_app/billing/Cargo.toml`
- [ ] `djogi-cli/tests/fixtures/adopter_app/billing/src/lib.rs` (Invoice)
- [ ] `djogi-cli/tests/fixtures/adopter_app/bin/Cargo.toml`
- [ ] `djogi-cli/tests/fixtures/adopter_app/bin/src/bin/djogi.rs` (djogi_main! with all 3 types)
- [ ] `djogi-cli/tests/fixtures/adopter_app_unforced/Cargo.toml` (virtual workspace root)
- [ ] `djogi-cli/tests/fixtures/adopter_app_unforced/.gitignore` (`target/`)
- [ ] `djogi-cli/tests/fixtures/adopter_app_unforced/tracker/Cargo.toml`
- [ ] `djogi-cli/tests/fixtures/adopter_app_unforced/tracker/src/lib.rs` (copy of tracker)
- [ ] `djogi-cli/tests/fixtures/adopter_app_unforced/billing/Cargo.toml`
- [ ] `djogi-cli/tests/fixtures/adopter_app_unforced/billing/src/lib.rs` (copy of billing)
- [ ] `djogi-cli/tests/fixtures/adopter_app_unforced/bin/Cargo.toml`
- [ ] `djogi-cli/tests/fixtures/adopter_app_unforced/bin/src/bin/djogi.rs` (hand-written, tracker only)
- [ ] `djogi-cli/tests/internal/adopter_linked_cli.rs` (test driver with all T-* tests)
- [ ] `djogi-cli/Cargo.toml` — add [[test]] registration for adopter_linked_cli

Modify these files:
- [ ] Rewrite `db.rs` docs_cmd_against_empty_inventory_succeeds test (exit 0 → exit 2)

Generate these artifacts:
- [ ] `adopter_app/Cargo.lock` (via cargo generate-lockfile)
- [ ] `adopter_app_unforced/Cargo.lock` (via cargo generate-lockfile)
