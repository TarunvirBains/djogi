# Plan v2 — Raw SQL Escape Hatch Harness (GH #133 + GH #134)

**Branch:** `harness/raw-methods-prevention` (off `origin/main` @ `8e007d2`).
**Worktree:** `/home/tarunvir/projects/djogi-harness/`.
**PR target:** `main`, **split into 3 PRs** (additive → refactor → lockdown).
**Cluster 8ζ disposition:** parked at `phase8-cluster-zeta-operational-tail`; rebases on PR 3's `main` after merge.

> **v2 changes from v1** (8 BLOCKs raised by fresh Opus + new design refinements):
> 1. `IntoAtomicScope` polymorphism preserved — `atomic()` is **not** reshaped. (BLOCK-1)
> 2. Honest cluster-8ζ rebase playbook in §10. (BLOCK-2)
> 3. New §5 enumerates 17 djogi-internal modules + the explicit `use` pattern for them. (BLOCK-3)
> 4. Single PR split into 3 PRs (additive → refactor → lockdown). (BLOCK-4)
> 5. JUSTIFICATION grep gate replaced by `cargo xtask check-justifications` (syn + line-anchored). (BLOCK-5)
> 6. Baseline reset to actual `origin/main` HEAD; references to `raw_methods_blacklist.rs` and `PENDING_CLEANUP_133` removed (those files arrive via 8ζ rebase, where 8ζ is responsible for their removal). (BLOCK-6)
> 7. Trait methods are `async fn` with explicit Send via `#[trait_variant::make]`, fixing the `RawCursorStream<'a>` lifetime pathology. (BLOCK-7)
> 8. Proc macro emits explicit error on `mod foo;` (file-loaded module without inline body). (BLOCK-8)
> 9. JUSTIFICATION format mandates `djogi#<n>` (cross-repo notation) so adopter gaps file on **djogi's** tracker, not theirs.
> 10. Refactor batches are dispatched as Sonnet subagents (one per phase prefix, oldest-first) — Codex reserved for novel mechanism work and adversarial review.

---

## 0. Goal and success criteria

**Goal:** make it structurally impossible for an ordinary integration test to bypass djogi's typed surface and reach raw SQL — at compile time, not at lint or runtime time.

**Success criteria** (every one must hold at PR 3 merge — earlier PRs hold subsets):

1. `cargo test --workspace` passes from a clean checkout against the standard local Postgres.
2. `cargo clippy --workspace --all-targets --features <explicit list> -- -D warnings` passes.
3. `cargo fmt --all -- --check` passes.
4. `cargo xtask check-justifications` passes (every `deliberately_bypass_convention_with_raw_sql` attribute under `tests/` is paired with a valid `JUSTIFICATION (djogi#<n>)` or `JUSTIFICATION (PIN)` comment within ±3 lines).
5. **Zero** ordinary integration tests under `tests/integration/` reference any of: `raw_query`, `raw_rows`, `raw_fetch_one`, `raw_scalar`, `raw_execute`, `raw_ddl`, `raw_stream`, `raw_stream_with_fetch_size`, `pool()`, `conn()`, `with_client`, `batch_execute`, or `tokio_postgres::` direct.
6. Every raw API has exactly one designated pin test under `tests/pin/`. Pin coverage matrix: 8 raw methods + pool/conn/with_client = at least 9 pin files (see §4).
7. The bypass attribute `#[djogi::deliberately_bypass_convention_with_raw_sql]` is the only public path to the raw API surface.
8. CLAUDE.md's "Tests must use djogi structs" section is rewritten to match the harness mechanism.
9. Cluster 8ζ rebases cleanly on PR 3's main; the rebase removes 8ζ's `raw_methods_blacklist.rs` runtime gate and its `PENDING_CLEANUP_133` allowlist (both now redundant — the harness supersedes them).

**Non-goals (deferred):**
- Adopter-side enforcement (their own clippy / CI). CLAUDE.md guidance only.
- `djogi lint` CLI subcommand (the xtask is the validator).
- `target/djogi_outbox/<table>_outbox.sql` build-time emission (a separate Phase 7 handoff).
- Notify watcher-died lifecycle gap (GH #131 — separate cluster).

---

## 0.0 Attribute name — `#[djogi::deliberately_bypass_convention_with_raw_sql]`

**Locked.** The verbose name is the design:
- **`deliberately`** — captures the intentional, conscious nature of the bypass. Adopters and AI agents must type it knowing exactly what it means; the word forces a moment of reflection.
- **`bypass`** — strong verb. There is a recommended path (the typed surface) and you are deliberately stepping around it.
- **`convention`** — pre-publish-accurate. djogi has *conventions* (recommended patterns: typed Models, sync_models, projection pipeline) on day one without claiming community-endorsed *idioms*. "Convention" is exactly the right word for the framework's recommended path before community adoption settles.
- **`with_raw_sql`** — names the *means* of bypass. Raw SQL is the **instrument** ("opened the door **with** a key"), not a route. `via_raw_sql` was considered and rejected — `via` carries a route/intermediary connotation that misframes raw SQL as a path one walks through rather than a tool one reaches for.

The `djogi::` namespace supplies the implicit "djogi's" — no need to repeat the name inside the attribute identifier.

---

## 0.1 Cultural framing — `unsafe`-style treatment

Raw SQL in djogi is intended to be culturally treated the way `unsafe` is in Rust: not banned, but always conscious. This shapes both the syntax and the convention.

**Mechanical parallels with `unsafe`:**

| `unsafe { ... }` in Rust | `#[djogi::deliberately_bypass_convention_with_raw_sql]` in djogi |
|---|---|
| Self-flagging at every use site | Same — verbose attribute name greppable per-call |
| Clippy / rustc encourage minimisation | Workspace clippy gates residual escape routes |
| Conventionally paired with `// SAFETY:` | Conventionally paired with `// JUSTIFICATION:` |
| Code review treats as a smell | Same |
| Every `unsafe` block declares an invariant | Every `deliberately_bypass_convention_with_raw_sql` declares why the typed surface is insufficient |
| Library authors minimise `unsafe` surface | djogi keeps the attribute confined to `tests/pin/` and rare adopter need |

**The `// JUSTIFICATION:` convention.** Every use site of the attribute (under `tests/`) must be accompanied by a comment explaining why the typed surface is insufficient. Form:

```rust
#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#234): citext column needs case-insensitive
// equality; QuerySet doesn't expose `LOWER(col) = LOWER($1)` yet.
async fn my_test(mut ctx: DjogiContext) {
    // ...
}
```

**Justification format** — exhaustive grammar:

| Form | Meaning | Where allowed |
|---|---|---|
| `// JUSTIFICATION (djogi#<n>): <reason>` | Filed as **djogi** GH issue #n — tracks the typed-surface gap upstream. | Any test under `tests/integration/` |
| `// JUSTIFICATION (PIN): exercises raw_<api> itself` | Pin test — the raw API IS what's being validated. | Only files under `tests/pin/` |

**Adopter-side filing rule (strict).** The `djogi#<n>` prefix uses GitHub's canonical cross-repo notation (renders as a clickable link from any repo). It forces the adopter to file the gap on **djogi's** tracker, not their own. The xtask validator (§1.6) rejects:
- `JUSTIFICATION (#234)` — bare `#` is ambiguous; could be the adopter's local issue.
- `JUSTIFICATION (myapp#234)` — wrong repo.
- `JUSTIFICATION (GH-234)` — wrong notation.
- `JUSTIFICATION:` (no parenthesised prefix) — required form is missing.

Error message emitted by the xtask:
```
JUSTIFICATION must reference djogi's issue tracker (`djogi#<n>`), not your
application's. Reaching for raw_* signals a gap in djogi's typed surface —
that gap belongs to djogi to fix. File at github.com/Tarunvir/djogi/issues,
then update the justification with the resulting issue number.

If you genuinely cannot file upstream (e.g. your raw SQL is for a
proprietary Postgres extension djogi will never wrap), open a discussion
at the same URL — we will either accept the case and assign you an issue
number, or document the carve-out and add a permitted form to the harness.
```

**Justifications double as a deficiency log.** Every non-pin justification names a typed-surface gap. The xtask emits a tally: total attribute uses, total tracked djogi issues. Over time the typed surface absorbs each issue and the corresponding attribute is removed. The ratchet shrinks toward "pin tests only".

**No "ergonomic raw SQL" surface.** djogi will never ship a `RawSqlBuilder` or a fluent `ctx.raw().execute(...)` shortcut. Every reach for raw SQL must walk through the verbose attribute and the JUSTIFICATION comment. Friction is the design.

---

## 0.2 Docs-first contract (implementation discipline)

**The docs ship before the code.** The implementer (Codex) does not write a single line of Rust until the spec and reference docs in the repo are updated to reflect this plan. The committed docs are the contract; the implementation must abide by them.

### 0.2.1 Doc surface that must update first

These artifacts ship as **commits 1–3 of PR 1**, before any code commit:

1. **`CLAUDE.md`** — root project file. Replace the current "Tests must use djogi structs, not raw escape hatches" section with the prose in §3.7. Add a forward reference to `docs/spec/raw-sql-escape-hatches.md`.

2. **`docs/spec/raw-sql-escape-hatches.md`** (new file) — the canonical specification of the harness:
   - The `unsafe`-style framing from §0.1.
   - The forbidden API inventory (8 raw methods, `pool()`, `conn()`, `with_client`, `batch_execute`, `tokio_postgres::*` direct).
   - The `RawAccessExt` trait shape from §1.1.
   - The `#[djogi::deliberately_bypass_convention_with_raw_sql]` attribute mechanics from §1.2.
   - The `// JUSTIFICATION (djogi#<n>):` convention from §0.1, including the adopter-side filing rule.
   - The pin-test directory structure from §4.
   - The xtask validator (§1.6) and how to invoke it locally.
   - The internal-callers pattern (§5) with the explicit `use crate::__bypass::RawAccessExt;` form.
   - The migration philosophy: "every reach for raw SQL walks through the verbose attribute and the justification. Friction is the design."
   - Pointers to `tests/pin/` and `tests/compile_fail/raw_sql/` as the canonical examples.

3. **`ReadMe.MD`** — public-facing project README. One-paragraph note that integration tests must use the typed surface; cross-link to `docs/spec/raw-sql-escape-hatches.md`.

4. **`docs/spec/decisions.md`** (existing) — append a numbered decision row recording: "Raw SQL is treated as djogi's `unsafe`. Use of `raw_*` requires the `#[djogi::deliberately_bypass_convention_with_raw_sql]` attribute and a `// JUSTIFICATION (djogi#<n>):` comment in tests. Tracked at GH #133." Date the decision.

5. **Module rustdoc on `djogi/src/__bypass.rs`** (lands with code commit but the prose itself is part of the contract — drafted in PR 1 commit 3 as a markdown file, attached to the module verbatim in the implementation commit). Specifies the trait's threat model, the seal, and the bypass-attribute requirement.

### 0.2.2 Verification: docs-implementation alignment

Every implementation commit must cite — in its commit message body — the spec section it enacts. Format: `Implements: docs/spec/raw-sql-escape-hatches.md §<n>` or `Implements: CLAUDE.md "Raw SQL is djogi's unsafe"`.

A docs commit that disagrees with a later implementation commit is a contract breach. The reviewer cycle (Codex/Gemini round-N) explicitly checks that the implementation matches the doc, not the other way around. If the implementer hits a real-world constraint that forces deviation, the doc is updated **first** with a follow-up commit, then the implementation lands.

### 0.2.3 Why docs-first (the rationale that goes in the spec)

Test fixtures hid the projection bug for years because the *behaviour* of `raw_execute` was easy to model in a fixture but its *purpose* (an escape hatch for cases the typed surface cannot reach) had drifted out of the documentation. New contributors and AI agents read the runtime surface, not the design intent.

By landing the spec first, the implementer is forced to read it. By landing the spec immutably (as a committed file referenced from CLAUDE.md), every subsequent change to the harness must update the spec — making drift a code-review-visible event, not a silent code-side accretion.

This is the "contract-driven development" pattern: the spec is the source of truth, the code is the obligation. It is intentionally heavier than a freewheeling implementation; that weight is the design, not friction to be optimised away.

---

## 1. Mechanism architecture

Six compile-time-anchored layers, in order of effect:

### 1.1 Sealed extension trait `RawAccessExt`

All raw escape methods move off `DjogiContext`'s public inherent surface onto a trait that lives in a `#[doc(hidden)]` module:

```rust
// djogi/src/__bypass.rs

mod sealed { pub trait Sealed {} }

/// Sealed extension trait that exposes djogi's raw SQL escape hatches.
/// Reachable only through `djogi::__bypass::RawAccessExt` after bringing
/// it into scope via `#[djogi::deliberately_bypass_convention_with_raw_sql]`
/// (in tests) or an explicit `use crate::__bypass::RawAccessExt;`
/// (inside djogi's own crates). The seal blocks foreign impls.
#[doc(hidden)]
#[trait_variant::make(RawAccessExt: Send)]
pub trait RawAccessExtBase: sealed::Sealed {
    async fn raw_query<T: FromPgRow>(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<T>, DjogiError>;

    async fn raw_rows(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<tokio_postgres::Row>, DjogiError>;

    async fn raw_fetch_one<T: FromPgRow>(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<T, DjogiError>;

    async fn raw_scalar<T>(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<T, DjogiError>
    where
        T: for<'b> FromSql<'b> + Send + 'static;

    async fn raw_execute(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<u64, DjogiError>;

    async fn raw_ddl(&mut self, sql: &str) -> Result<(), DjogiError>;

    async fn raw_stream<'a>(
        &'a mut self,
        sql: &'a str,
        params: &'a [&'a (dyn ToSql + Sync)],
    ) -> Result<RawCursorStream<'a>, DjogiError>;

    async fn raw_stream_with_fetch_size<'a>(
        &'a mut self,
        sql: &'a str,
        params: &'a [&'a (dyn ToSql + Sync)],
        fetch_size: u32,
    ) -> Result<RawCursorStream<'a>, DjogiError>;
}

impl sealed::Sealed for crate::DjogiContext {}
impl RawAccessExt for crate::DjogiContext { /* current bodies, moved verbatim */ }
```

**Why `async fn` + `trait_variant::make`** (replaces v1's `impl Future + Send + 'a` form):
- v1's hand-written `impl Future<Output = Result<RawCursorStream<'a>, _>> + Send + 'a` ties the future's bound, the borrow's lifetime, and the Send opt-in into one expression. For methods returning a self-borrowing stream (`RawCursorStream<'a>`), rustc's HRTB inference is fragile; in practice the hand-written form requires extra `for<'a>` bounds at every call site to compile.
- `async fn` in traits is stable as of Rust 1.75. Lifetime capture is automatic — the `'a` flows through correctly.
- `#[trait_variant::make(RawAccessExt: Send)]` produces both `RawAccessExtBase` (no Send bound) and `RawAccessExt: Send` variants. The `Send` variant is the public-facing trait; `Send` is required for djogi's tokio runtime.
- `trait_variant` is a tiny crate (one macro), already in djogi's dependency closure via tokio.

**Companion trait `RawPoolAccessExt`** — same shape, exposes pool-level raw access without breaking `IntoAtomicScope`:

```rust
#[doc(hidden)]
#[trait_variant::make(RawPoolAccessExt: Send)]
pub trait RawPoolAccessExtBase: sealed::Sealed {
    /// Direct pool access. Returns `None` outside a pool-bearing context
    /// (rare — only certain unit-test contexts construct `DjogiContext`
    /// without a pool).
    fn raw_pool(&self) -> Option<&DjogiPool>;

    /// Direct connection access. Returns `None` if the context is not
    /// currently holding a connection (e.g. between transactions).
    fn raw_conn(&mut self) -> Option<&mut PgConnection>;

    /// Run a closure with a borrowed `tokio_postgres::Client`.
    /// Mirrors `DjogiPool::with_client` but routed through the bypass
    /// trait so it remains gated.
    async fn raw_with_client<F, R>(&self, f: F) -> Result<R, DjogiError>
    where
        F: for<'c> FnOnce(&'c mut tokio_postgres::Client) -> BoxFuture<'c, Result<R, DjogiError>> + Send,
        R: Send + 'static;
}
```

The trait is `#[doc(hidden)]` (no rustdoc surface), the sealed marker is `pub(crate)` (no foreign impls possible), and the module is reachable only as `djogi::__bypass::{RawAccessExt, RawPoolAccessExt}`. Without the trait in scope, methods of those names do not resolve.

### 1.2 Bypass attribute proc macro

```rust
// djogi-macros/src/raw_bypass.rs

/// Brings `djogi::__bypass::{RawAccessExt, RawPoolAccessExt}` into scope
/// for the decorated item, unlocking direct access to djogi's raw SQL
/// escape hatches. Every use is auditable via `git grep`.
///
/// Without this attribute, `raw_*`, `raw_pool()`, `raw_conn()`,
/// `raw_with_client`, and `batch_execute` are unreachable on
/// `DjogiContext` and `DjogiPool`. The verbose name is the design:
/// every use site is self-flagging in code review and resists the
/// "easy path" reflex an AI agent might apply.
#[proc_macro_attribute]
pub fn deliberately_bypass_convention_with_raw_sql(_attr: TokenStream, item: TokenStream) -> TokenStream { ... }
```

Decoration accepted on:
- Free `fn` items (most common case — pin tests).
- `impl` blocks (when a method needs raw access).
- `mod` items **with inline body** (rare — entire module legitimately needs raw access).

**File-loaded `mod foo;` form is rejected** with a clear error (see §3.3 for the explicit error path; this is BLOCK-8 from v1 review). Decoration on items where injection has no meaning (`struct`, `enum`, `const`, `static`, `type`, `use`) is also rejected with a span-precise compile error.

### 1.3 `pool()` and `conn()` escape-route demotion (PR 3)

Today `DjogiContext::pool()` and `DjogiContext::conn()` are `pub` (`djogi/src/context.rs:272, 283`). Adopters can write:

```rust
ctx.pool().unwrap().with_client(|c| async { c.execute(sql, &[]).await }).await
```

— bypassing every gate. Demote both methods to `pub(crate)`. The `RawPoolAccessExt` trait gains `raw_pool(&self) -> Option<&DjogiPool>` and `raw_conn(&mut self) -> Option<&mut PgConnection>` so the bypass attribute is the unlock for these too.

`DjogiPool::with_client` (`djogi/src/pg/pool.rs:378`): same demotion — `pub` → `pub(crate)`, with `RawPoolAccessExt::raw_with_client` as the gated unlock.

`DjogiContext::batch_execute` is already `pub(crate)` on origin/main (`djogi/src/context.rs:645`); no change needed.

`tokio_postgres::Client::*`: cannot be hidden by djogi's type system (it's an external crate's public API). Belt-and-suspenders via clippy lint (§1.5).

**Demotion ships in PR 3, not PR 1.** PR 1 adds the harness additively (the trait + attribute exist; `raw_*` remains `pub`). PR 2 refactors all 54 tests off `raw_*`. PR 3 flips `raw_*` to `pub(crate) sealed-trait-only` — that is when bare `ctx.raw_execute(...)` stops compiling. The split makes each PR bisectable and isolates the API-break to a single moment.

### 1.4 `transaction::atomic` — preserved polymorphism (NOT reshaped)

**v1 §1.4 was wrong.** It proposed reshaping `atomic` to take `&mut DjogiContext` and extract the pool internally. That collapses the existing `IntoAtomicScope` trait (sealed at `djogi/src/transaction.rs:75`), which today has TWO impls:

```rust
// djogi/src/transaction.rs:93
impl IntoAtomicScope for &DjogiPool { /* outermost — opens new tx */ }

// djogi/src/transaction.rs:144
impl IntoAtomicScope for &mut DjogiContext { /* nested — opens savepoint */ }
```

This polymorphism is load-bearing: outermost transaction opens a fresh BEGIN; nested calls open SAVEPOINTs. Reshaping `atomic` to a single signature destroys nested-savepoint dispatch.

**v2 plan: keep `atomic` exactly as-is.** Pin tests that need outermost transactions reach the pool through the bypass attribute:

```rust
#[djogi::djogi_test]
#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (PIN): exercises raw_pool + atomic outermost-scope binding.
async fn raw_pool_outermost_atomic(mut ctx: djogi::DjogiContext) {
    // RawPoolAccessExt is in scope thanks to the attribute
    let pool = ctx.raw_pool().expect("pool-bearing test context");
    djogi::transaction::atomic(pool, |inner| Box::pin(async move {
        let n = inner.raw_execute("SELECT 1", &[]).await?;  // RawAccessExt also in scope
        Ok(n)
    })).await.expect("atomic round-trip");
}
```

Ordinary tests using `atomic(&mut ctx, ...)` continue working unchanged — the `&mut DjogiContext` impl of `IntoAtomicScope` is untouched.

### 1.5 Clippy `disallowed_methods` for residual escape routes

Add workspace `clippy.toml`:

```toml
disallowed-methods = [
    { path = "tokio_postgres::Client::query", reason = "Tests must use Model::objects() or `#[djogi::deliberately_bypass_convention_with_raw_sql]`" },
    { path = "tokio_postgres::Client::execute", reason = "Tests must use Model::create / Model::save / Model::delete or `#[djogi::deliberately_bypass_convention_with_raw_sql]`" },
    { path = "tokio_postgres::Client::batch_execute", reason = "Tests must use #[djogi_test(sync_models = [...])] or `#[djogi::deliberately_bypass_convention_with_raw_sql]`" },
    { path = "tokio_postgres::Client::query_one", reason = "Same as `query`" },
    { path = "tokio_postgres::Client::query_opt", reason = "Same as `query`" },
    { path = "tokio_postgres::connect", reason = "Tests must not connect outside djogi's pool. Use #[djogi_test]." },
    { path = "deadpool_postgres::Pool::get", reason = "Use djogi::transaction::atomic and the typed Model surface" },
]
```

Per-crate override in `djogi-cli/Cargo.toml`, `djogi-shell/Cargo.toml`, and selected modules in `djogi/src/migrate/` (via `#[allow(clippy::disallowed_methods)]` on specific functions) for legitimate internal usage. See §5 for the full djogi-internal callers list.

CI runs `cargo clippy --workspace --all-targets -- -D warnings` so violations are build-failures.

### 1.6 `cargo xtask check-justifications` (replaces v1's grep gate)

v1 §0.1 line 125 proposed a grep gate (`git grep -E 'JUSTIFICATION \(#[0-9]+\)' | wc -l`). This is structurally weak (BLOCK-5):
- Cannot distinguish a JUSTIFICATION 30 lines away from one adjacent to the attribute.
- Cannot distinguish a JUSTIFICATION inside a string literal from a real comment.
- Cannot validate the parenthesised prefix shape.

v2 introduces a `cargo xtask` validator that walks the source tree:

```rust
// xtask/src/check_justifications.rs

/// Validates that every `#[djogi::deliberately_bypass_convention_with_raw_sql]`
/// attribute under `tests/` is paired with a valid JUSTIFICATION comment
/// within ±3 lines.
///
/// Discovery: walk every `.rs` file under `tests/` (skipping `target/`).
/// Use `syn::parse_file` to find every `Attribute` whose path resolves to
/// the bypass attribute. Record (file, line) for each.
///
/// Validation: for each (file, line), read the source file, scan ±3 lines
/// for a comment matching one of:
///   - `// JUSTIFICATION (djogi#<digits>): <reason>`  (any non-pin test)
///   - `// JUSTIFICATION (PIN): exercises raw_<api> itself` (pin tests only)
///
/// Plain-English grammar (no Rust regex per project policy):
///   1. Strip leading whitespace.
///   2. Match literal `// JUSTIFICATION (`.
///   3. Either: literal `djogi#` + 1+ ASCII digits + literal `): ` + non-empty reason
///      Or:    literal `PIN): ` + non-empty reason (only allowed for files under tests/pin/)
///   4. Reject anything else.
///
/// Output: zero exit code on success; non-zero with a per-violation report
/// on failure. The error message for a malformed JUSTIFICATION is the
/// adopter-side filing rule from §0.1.
pub fn run() -> ExitCode { ... }
```

**Implementation approach** (Codex chooses the exact form, but must satisfy):
- `syn` for attribute discovery — never miss an attribute regardless of indentation, multi-line attribute lists, or macro-generated code with span info.
- Source-file line walk for comment proximity — the syn AST does not carry comments, so the validator reads the raw file and inspects ±3 lines around the attribute's line span.
- The grammar is enforced via byte-level checks (`u8::is_ascii_digit`, slice equality) per project no-regex policy. Spell out the rule in plain English in the rustdoc.

**Scope limitation.** The xtask scans **only `tests/`**. Internal djogi callers (§5) use the trait via `use crate::__bypass::RawAccessExt;` directly — they are framework-internal and do not require JUSTIFICATION comments. The xtask does not flag them.

**CI integration.** PR 1 wires the xtask into `.github/workflows/ci.yml`:

```yaml
- name: Validate JUSTIFICATION comments
  run: cargo xtask check-justifications
```

Local pre-commit hooks (per `feedback_precommit_checks.md`) gain the same step.

---

## 2. File structure changes

### 2.1 New files (PR 1)

```
djogi/src/__bypass.rs                         # sealed RawAccessExt + RawPoolAccessExt
djogi-macros/src/raw_bypass.rs                # the deliberately_bypass_convention_with_raw_sql proc macro
xtask/Cargo.toml                              # cargo xtask binary (workspace member)
xtask/src/main.rs                             # xtask CLI dispatch
xtask/src/check_justifications.rs             # the validator from §1.6
clippy.toml                                   # workspace-level disallowed_methods config
docs/spec/raw-sql-escape-hatches.md           # canonical spec (§0.2.1 item 2)
docs/spec/internal/__bypass-rustdoc-draft.md  # rustdoc draft (§0.2.1 item 5)
tests/pin/raw_execute_pin.rs                  # 9 pin files, one per raw API + raw_pool_access
tests/pin/raw_query_pin.rs
tests/pin/raw_rows_pin.rs
tests/pin/raw_fetch_one_pin.rs
tests/pin/raw_scalar_pin.rs
tests/pin/raw_ddl_pin.rs
tests/pin/raw_stream_pin.rs
tests/pin/raw_stream_with_fetch_size_pin.rs
tests/pin/raw_pool_access_pin.rs              # raw_pool + raw_conn + raw_with_client
```

### 2.2 New files (PR 3)

```
tests/compile_fail/raw_sql/ordinary_raw_execute_fails.rs
tests/compile_fail/raw_sql/ordinary_raw_query_fails.rs
tests/compile_fail/raw_sql/ordinary_raw_ddl_fails.rs
tests/compile_fail/raw_sql/ordinary_pool_access_fails.rs
tests/compile_fail/raw_sql/ordinary_with_client_fails.rs
tests/compile_fail/raw_sql/ordinary_tokio_postgres_direct_fails.rs
tests/compile_fail/raw_sql_compile_fail.rs    # trybuild driver
```

These ship in PR 3 because they assert non-resolution of `raw_*` — only true after the demotion lands.

### 2.3 Modified files

**PR 1** (additive):
```
djogi/src/lib.rs                              # pub mod __bypass; (additive)
djogi-macros/src/lib.rs                       # export deliberately_bypass_convention_with_raw_sql
djogi/Cargo.toml                              # new [[test]] entries for tests/pin/*
Cargo.toml (workspace root)                   # workspace.lints.clippy = workspace + xtask member
CLAUDE.md                                     # rule rewrite (§3.7)
ReadMe.MD                                     # one-paragraph harness note
docs/spec/decisions.md                        # new decision row
.github/workflows/ci.yml                      # add trybuild step (deferred-firing until PR 3) + xtask step
```

**PR 2** (test refactor; no library API changes):
```
tests/integration/phase{1..8}_*.rs            # 54 files refactored; raw_* call sites removed
```

**PR 3** (lockdown):
```
djogi/src/context.rs                          # raw_* methods demoted; impl moved to __bypass.rs
djogi/src/pg/pool.rs                          # with_client demoted; RawPoolAccessExt impl
djogi/src/{descriptor,expr,query,testing,migrate/*,live_migrate/*,outbox/publishers/notify}.rs
                                               # internal callers add `use crate::__bypass::RawAccessExt;`
djogi-cli/src/{analyze,live,verify}.rs        # internal callers add `use djogi::__bypass::RawAccessExt;`
djogi-cli/Cargo.toml                          # [lints.clippy.disallowed-methods] = "allow"
djogi-shell/Cargo.toml                        # same
.github/workflows/ci.yml                      # trybuild gate goes live
```

### 2.4 Files NOT touched on this branch

- `tests/integration/raw_methods_blacklist.rs` — does not exist on `origin/main`. It exists on cluster 8ζ (`phase8-cluster-zeta-operational-tail`). When 8ζ rebases on PR 3's main, 8ζ's rebase commit removes the file (the harness supersedes it). See §10.4.

- `PENDING_CLEANUP_133` allowlist constant — same disposition; lives in 8ζ's `raw_methods_blacklist.rs`, removed at 8ζ's rebase.

`tests/integration/phase5_zero_raw_in_atomic.rs` is **moved** to `tests/pin/raw_execute_and_scalar_pin.rs` in PR 2 (after the file's 8 raw-method-using siblings refactor onto the typed surface). Decorated with `#[djogi::deliberately_bypass_convention_with_raw_sql]` and given a `JUSTIFICATION (PIN)` comment.

---

## 3. Code shape (concrete)

### 3.1 `djogi/src/__bypass.rs`

(See §1.1 — full trait definition.)

### 3.2 `djogi/src/context.rs` after PR 3 surgery

```rust
impl DjogiContext {
    // Typed surface — unchanged public API.
    // (Unchanged methods: model save/create/delete dispatch, transactional
    // helpers, etc. — none of these call raw_* externally.)

    // pub(crate) — internal usage by macros, runner, refresh, sync_models.
    pub(crate) fn __bypass_pool(&self) -> Option<&DjogiPool> { self.pool.as_ref() }
    pub(crate) fn __bypass_conn(&mut self) -> Option<&mut PgConnection> { ... }

    // No public raw_* inherent methods. RawAccessExt holds the bodies
    // (see djogi/src/__bypass.rs).
}
```

The current `pub async fn raw_*` bodies (`context.rs:864–1163`) move verbatim into the `impl RawAccessExt for DjogiContext` block in `__bypass.rs`. No body changes.

### 3.3 `djogi-macros/src/raw_bypass.rs` (with explicit error on file-loaded mod)

```rust
use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, parse_quote, Item};

#[proc_macro_attribute]
pub fn deliberately_bypass_convention_with_raw_sql(
    _attr: TokenStream,
    item: TokenStream,
) -> TokenStream {
    let item = parse_macro_input!(item as Item);
    let injected_access: syn::Stmt = parse_quote!(use ::djogi::__bypass::RawAccessExt;);
    let injected_pool: syn::Stmt = parse_quote!(use ::djogi::__bypass::RawPoolAccessExt;);

    match item {
        Item::Fn(mut f) => {
            f.block.stmts.insert(0, injected_access);
            f.block.stmts.insert(1, injected_pool);
            quote! { #f }.into()
        }
        Item::Impl(mut i) => {
            for impl_item in &mut i.items {
                if let syn::ImplItem::Fn(method) = impl_item {
                    method.block.stmts.insert(0, injected_access.clone());
                    method.block.stmts.insert(1, injected_pool.clone());
                }
            }
            quote! { #i }.into()
        }
        Item::Mod(mut m) => {
            // BLOCK-8 fix: file-loaded modules (`mod foo;`) cannot have
            // statements injected. Reject explicitly rather than silently
            // producing an unmodified output.
            let Some((_, contents)) = m.content.as_mut() else {
                return syn::Error::new_spanned(
                    &m,
                    "`#[djogi::deliberately_bypass_convention_with_raw_sql]` cannot decorate a \
                     file-loaded module declaration (`mod foo;`). Either inline the module body \
                     (`mod foo { ... }`) and decorate that, or attach the attribute to specific \
                     `fn` or `impl` items inside the module's source file.",
                )
                .to_compile_error()
                .into();
            };
            // Inject at item-level (not stmt-level) for inline modules.
            let access_use: syn::Item = parse_quote!(use ::djogi::__bypass::RawAccessExt;);
            let pool_use: syn::Item = parse_quote!(use ::djogi::__bypass::RawPoolAccessExt;);
            contents.insert(0, access_use);
            contents.insert(1, pool_use);
            quote! { #m }.into()
        }
        other => syn::Error::new_spanned(
            other,
            "`#[djogi::deliberately_bypass_convention_with_raw_sql]` may only decorate `fn`, \
             `impl`, or `mod` (with inline body) items.",
        )
        .to_compile_error()
        .into(),
    }
}
```

### 3.4 Pin test example

```rust
// tests/pin/raw_execute_pin.rs

use djogi::prelude::*;

#[djogi::djogi_test]
#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (PIN): exercises raw_execute round-trip; this IS the API being validated.
async fn raw_execute_round_trips_inside_atomic(mut ctx: djogi::DjogiContext) {
    djogi::transaction::atomic(&mut ctx, |inner| Box::pin(async move {
        let n = inner.raw_execute("SELECT 1", &[]).await?;
        Ok(n)
    })).await.expect("raw_execute round-trip");
}
```

### 3.5 Compile-fail fixture example (PR 3)

```rust
// tests/compile_fail/raw_sql/ordinary_raw_execute_fails.rs

use djogi::prelude::*;

#[djogi::djogi_test]
async fn ordinary_test_must_not_call_raw_execute(mut ctx: djogi::DjogiContext) {
    ctx.raw_execute("SELECT 1", &[]).await.unwrap();
}

fn main() {}
```

Expected `.stderr` (rustc 1.75+ format):

```
error[E0599]: no method named `raw_execute` found for struct `djogi::DjogiContext`
  --> tests/compile_fail/raw_sql/ordinary_raw_execute_fails.rs:6:9
   |
6  |     ctx.raw_execute("SELECT 1", &[]).await.unwrap();
   |         ^^^^^^^^^^^ method not found
   |
   = help: the method is provided by the `djogi::__bypass::RawAccessExt`
           trait but is not in scope (and the trait is `#[doc(hidden)]`
           because it is intentionally hard to reach)
   = help: decorate this function with #[djogi::deliberately_bypass_convention_with_raw_sql]
           to bring the trait into scope (only do this in tests/pin/), or use the typed
           surface: Model::create / Model::save / Model::delete / Model::objects()
```

### 3.6 `clippy.toml` (workspace root)

(See §1.5.)

### 3.7 CLAUDE.md prose (drop-in replacement for the current "Tests must use djogi structs" section)

```markdown
## Raw SQL is djogi's `unsafe`

Raw SQL in djogi is treated culturally the way `unsafe` is in Rust: not
banned, but always conscious. The mechanism enforces this at compile
time; the convention enforces it in code review.

**The mechanism.** The raw SQL escape hatches (`raw_execute`,
`raw_query`, `raw_rows`, `raw_fetch_one`, `raw_scalar`, `raw_ddl`,
`raw_stream`, `raw_stream_with_fetch_size`) live on the
`djogi::__bypass::RawAccessExt` trait and are unreachable from
`DjogiContext` without the bypass attribute. `pool()`, `conn()`,
`with_client`, and `batch_execute` are similarly gated. Direct use of
`tokio_postgres::Client` or `deadpool_postgres::Pool` is gated by a
workspace `clippy::disallowed_methods` lint.

**The bypass attribute.** To use any raw escape — typically in a
dedicated pin test under `tests/pin/`, or in a deliberately
unidiomatic helper — decorate the enclosing item:

    #[djogi::deliberately_bypass_convention_with_raw_sql]
    // JUSTIFICATION (djogi#234): citext column needs case-insensitive
    // equality; QuerySet doesn't expose `LOWER(col) = LOWER($1)` yet.
    async fn my_test(mut ctx: DjogiContext) { ... }

**The `// JUSTIFICATION (djogi#<n>):` convention.** Every use of the
attribute under `tests/` MUST be paired with a `JUSTIFICATION` comment
within ±3 lines, validated by `cargo xtask check-justifications`. The
issue number references **djogi's** tracker (`djogi#<n>` is GitHub
cross-repo notation), not your application's — reaching for raw_*
signals a gap in djogi's typed surface, and that gap belongs to djogi
to fix.

**Pin tests** under `tests/pin/` use `JUSTIFICATION (PIN): exercises
raw_<api> itself` instead of an issue number. Pin tests are the
legitimate carve-out — one per raw API.

**Ordinary tests.** Every other integration test under
`tests/integration/` must exercise the typed surface: `Model::create`,
`Model::save`, `Model::delete`, `Model::objects()`,
`djogi::transaction::atomic`, and `#[djogi::djogi_test(sync_models = [...])]`.

**No ergonomic raw SQL.** djogi will not ship a fluent `ctx.raw().execute(...)`
shortcut or a `RawSqlBuilder`. Every reach for raw SQL walks through the
verbose attribute and the justification. Friction is the design.

The harness has no runtime grep gate; the type system, clippy, and the
xtask validator are the enforcement. See `docs/spec/raw-sql-escape-hatches.md`
for the full specification.
```

---

## 4. Pin test inventory

Each raw API gets exactly one pin test. The migration also moves the existing `phase5_zero_raw_in_atomic.rs` content into the appropriate pin file(s).

| Pin file | Coverage |
|---|---|
| `tests/pin/raw_execute_pin.rs` | `raw_execute` round-trip + atomic-transaction participation (existing fixture content) |
| `tests/pin/raw_query_pin.rs` | `raw_query` typed-row decode |
| `tests/pin/raw_rows_pin.rs` | `raw_rows` raw `tokio_postgres::Row` |
| `tests/pin/raw_fetch_one_pin.rs` | `raw_fetch_one` exactly-one row |
| `tests/pin/raw_scalar_pin.rs` | `raw_scalar` typed scalar (existing fixture content) |
| `tests/pin/raw_ddl_pin.rs` | `raw_ddl` simple-query DDL |
| `tests/pin/raw_stream_pin.rs` | `raw_stream` cursor stream default fetch size |
| `tests/pin/raw_stream_with_fetch_size_pin.rs` | `raw_stream_with_fetch_size` custom fetch size |
| `tests/pin/raw_pool_access_pin.rs` | `raw_pool` + `raw_conn` + `raw_with_client` round-trip |

Each file decorated with `#[djogi::deliberately_bypass_convention_with_raw_sql]` and a `JUSTIFICATION (PIN)` comment. Each registered as `[[test]]` in `djogi/Cargo.toml`.

---

## 5. Internal djogi callers (BLOCK-3)

The harness must not break djogi's own crates. 17 modules currently call `raw_*`, `pool()`, `conn()`, `with_client`, or `batch_execute` (75 call sites total, enumerated from `origin/main` HEAD on 2026-05-06):

### 5.1 Modules in `djogi/src/`

```
djogi/src/context.rs                 # the trait impl source — special case, lives in __bypass.rs after PR 3
djogi/src/descriptor.rs              # raw_query for descriptor introspection
djogi/src/expr/mod.rs                # raw_scalar for expression evaluation
djogi/src/live_migrate/daemon.rs     # raw_ddl + raw_execute for live migration apply
djogi/src/live_migrate/state.rs      # raw_query + raw_execute for live state I/O
djogi/src/migrate/audit.rs           # raw_execute for audit table writes
djogi/src/migrate/bootstrap.rs       # raw_ddl for Phase 0 bootstrap
djogi/src/migrate/ledger.rs          # raw_query + raw_execute for ledger I/O
djogi/src/migrate/repair.rs          # raw_ddl + raw_query for schema repair
djogi/src/migrate/runner.rs          # raw_execute for migration apply (heavy use)
djogi/src/migrate/seed.rs            # raw_execute for seed runs
djogi/src/outbox/publishers/notify.rs  # raw_execute for NOTIFY emission
djogi/src/query/field.rs             # raw_query for ad-hoc field-level queries
djogi/src/testing.rs                 # raw_ddl for test scaffolding (sync_models internals)
```

### 5.2 Modules in `djogi-cli/src/`

```
djogi-cli/src/analyze.rs   # raw_query for schema analysis
djogi-cli/src/live.rs      # raw_ddl + raw_query for live commands
djogi-cli/src/verify.rs    # raw_query for verification
```

### 5.3 Internal-callers pattern (PR 3)

Internal callers do **not** use the `#[djogi::deliberately_bypass_convention_with_raw_sql]` attribute. The attribute is a discoverability aid for tests and adopters; for crate-internal code, a plain `use` is cleaner:

```rust
// djogi/src/migrate/runner.rs (and 13 sibling modules in djogi/src/)

use crate::__bypass::RawAccessExt;
// (and `use crate::__bypass::RawPoolAccessExt;` if pool-level access is used)

// raw_execute, raw_query, etc. now resolve as before.
```

```rust
// djogi-cli/src/analyze.rs (and 2 sibling modules)

use djogi::__bypass::RawAccessExt;

// raw_query etc. now resolve as before.
```

The `__bypass` module is `pub` (so other djogi crates can import it) but `#[doc(hidden)]` (so it doesn't surface in rustdoc). The seal blocks foreign impls — adopters of djogi cannot create their own `RawAccessExt`-implementing types.

### 5.4 Internal-callers don't need JUSTIFICATION

The xtask scans only `tests/`. Internal `djogi/src/` and `djogi-cli/src/` files are exempt from JUSTIFICATION-comment validation by design — they are framework-internal and the audit log is not the right surface for djogi's own substrate use.

This decision is reversible: a future xtask flag could scan internal files too if internal raw SQL becomes a code-review concern. For PR 3, the limit is `tests/`.

### 5.5 Internal-callers ship in PR 3

The 17 modules add their `use` statements in the same commit that demotes `raw_*` from inherent `pub` to sealed `pub(crate)` extension trait. Within PR 3, the commit order:

1. Add `use crate::__bypass::RawAccessExt;` to all 17 modules — no behaviour change yet (the `use` shadows nothing; raw_* is still resolvable via the inherent impl).
2. Move the `raw_*` bodies from inherent `impl DjogiContext` to `impl RawAccessExt for DjogiContext` in `__bypass.rs`; demote inherent methods to `pub(crate)` (or remove entirely — only the trait impl remains).
3. Demote `pool()`, `conn()`, `DjogiPool::with_client` to `pub(crate)`.
4. Land the trybuild compile-fail fixtures.

The order makes step 2 a one-commit atomic API break — bisectable to a single hash.

---

## 6. Migration: 54 ordinary integration tests (PR 2)

### 6.1 Phase order (oldest first)

phase1 → phase2 → phase3 → phase4 → phase4_5 → phase5 → phase5_5 → phase5_zero → phase6 → phase6_5 → phase7 → phase7_5 → phase7_zero → phase7_zero2 → phase8 → phase8_zero

### 6.2 Per-test playbook

For each violating file:

1. Read the test end-to-end. Identify every `raw_*` / `pool()` / `with_client` call site.
2. Map raw operations to typed equivalents:
   - `raw_execute("CREATE TABLE ...")` → `#[djogi::djogi_test(sync_models = [Model])]` on the test (drops the call entirely).
   - `raw_execute("INSERT ...")` → `Model::create` (or bulk via `Model::bulk_create`).
   - `raw_query("SELECT ...")` → `Model::objects().filter(...).fetch_all(&mut ctx)`.
   - `raw_scalar("SELECT COUNT(*)")` → `Model::objects().count(&mut ctx)`.
   - `raw_execute("UPDATE ...")` → `model.save(&mut ctx)` after mutation.
   - `raw_execute("DELETE ...")` → `model.delete(&mut ctx)` or `Model::objects().filter(...).delete(&mut ctx)`.
   - `raw_execute("TRUNCATE ...")` → drop entirely (per-test DB is fresh from `#[djogi_test]`).
   - `pool().clone()` + `atomic(&pool, ...)` → `atomic(&mut ctx, ...)` if no pool-level need; else move to `tests/pin/` with the bypass attribute.
3. If the test uses `#[model(events)]`: the `_outbox` table must be projected by `sync_models` — depends on §7 (GH #134 sub-task), which lands in PR 1.
4. If the typed surface genuinely cannot express the test's need, **stop and surface the gap**: file a djogi GH issue, then add the bypass attribute + `JUSTIFICATION (djogi#<n>)` and move the test to `tests/pin/` (NOT `tests/integration/`). This should be rare; the canonical recipes above cover ~98% of the 54 files.
5. Run the single test locally: `DATABASE_URL=postgres://djogi:djogi@localhost:5432/djogi_test cargo test -p djogi --test <name>`.
6. Commit the refactor. Per the atomic-commits memory, prefer one commit per test file; per phase batch is acceptable when the changes are mechanical and co-located.

### 6.3 Sonnet-subagent batching (the dispatch model)

PR 2 is mechanical pattern-matching at scale. Codex `--effort xhigh` is the wrong tool — too expensive, too slow for repetitive work. Sonnet handles each phase batch:

| Batch | Subject | Files | Why grouped |
|---|---|---|---|
| 1 | phase1 | `phase1_model.rs` | Simplest schema, sets pattern |
| 2 | phase2 | `phase2_queryset.rs` | QuerySet patterns share idiom |
| 3 | phase3 | `phase3_relations.rs` | FK setup is its own pattern |
| 4 | phase4 | `phase4_transactions_expressions.rs` | Atomic-scope-touching |
| 5 | phase5 | `phase5_5_auth.rs`, `phase5_fts.rs`, `phase5_postgres_native.rs`, `phase5_streaming.rs` | Auth / FTS / streaming / native — share Postgres-feature idioms |
| 6 | phase5_zero | `phase5_zero_raw_in_atomic.rs` | The atomic-pin source — **moved** to tests/pin/ |
| 7 | phase6 | `phase6_5_aggregates.rs`, `phase6_5_spatial_polish.rs`, `phase6_spatial.rs` | Aggregates + spatial |
| 8 | phase7 | `phase7_t4_runner_live.rs`, `phase7_t5_repair_verify_live.rs`, `phase7_t7_policy_attune_live.rs`, `phase7_t8_seed_docs_live.rs`, `phase7_t9_pk_flip_live.rs`, `phase7_t10_sync_models_live.rs`, `phase7_t10_sync_models_parity.rs`, `phase7_zero_indexes_live.rs` | Migration system — heaviest batch (8 files) |
| 9 | phase7_5 | `phase7_5_backfill_live.rs`, `phase7_5_pr7_exclusion_generated_live.rs`, `phase7_5_t111_rls_fk_tenant_key_live.rs` | Phase 7.5 polish |
| 10 | phase7_zero2 | 7 files (`phase7_zero2_*`) | Schema-zero variants |
| 11 | phase8 compose+hooks+role | `phase8_compose_*.rs`, `phase8_hooks_*.rs`, `phase8_set_role_*.rs` | Composition + lifecycle hooks (8 files) |
| 12 | phase8 t7+t8 (Punnu) | `phase8_t7_3..t7_6_*.rs`, `phase8_t8_4..t8_10_*.rs` | Punnu cluster (10 files) |
| 13 | phase8_zero | `phase8_zero_*.rs` | Cluster-C bench/spatial/window (6 files) |

Each Sonnet subagent receives:
- The harness API surface (attribute + `djogi_test(sync_models=[...])`).
- File list for its batch.
- Canonical refactor recipes from §6.2.
- Instruction: when the typed API genuinely doesn't exist, stop and surface the gap to the orchestrator (Claude), who files the djogi GH issue. The subagent then writes `// JUSTIFICATION (djogi#<n>):` against that fresh issue.
- Per-file commit (atomic-commits memory).
- `cargo test --test <test_name>` must pass before handoff.

Codex stays reserved for: PR 1 (novel mechanism), PR 3 (lockdown semantics + internal-callers sweep), and adversarial review on every Sonnet batch's output.

### 6.4 Verification gate per phase batch

After each Sonnet batch's refactor, the orchestrator runs:

- `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings` — must pass.
- `DATABASE_URL=... cargo test --workspace` — must pass.
- `git grep -E '\.raw_(query|rows|fetch_one|scalar|execute|ddl|stream)\(' tests/integration/` — must shrink each batch.
- `cargo xtask check-justifications` — must pass (any new pin-tests added during the batch have valid JUSTIFICATIONs).

### 6.5 Events-bearing tests blocker (resolved by PR 1's GH #134 sub-task)

Tests using `#[model(events)]` cannot refactor onto `#[djogi_test(sync_models=[Model])]` until projection synthesizes the `{table}_outbox` companion (GH #134). This sub-task lands in **PR 1** (§7 below). PR 2 starts only after PR 1 merges, so all events-bearing tests are unblocked from batch 1.

Files affected: identify with `git grep -lE '#\[model[^]]*\bevents\b' tests/integration/`.

---

## 7. Sub-task: GH #134 — projection synthesizes `{table}_outbox` (PR 1)

This sub-task ships in **PR 1** (the additive harness PR) because it is a precondition for refactoring events-bearing tests in PR 2.

### 7.1 Change

In `djogi/src/migrate/projection.rs`, when iterating `desc.has_outbox == true` models, synthesize a sibling `{m.table_name}_outbox` `TableSchema` in the same bucket. Column shape:

```
id         BIGINT      PRIMARY KEY DEFAULT heerid_next()
row_id     <pk-sql-type-of-source>  NOT NULL
action     TEXT        NOT NULL
payload    JSONB       NOT NULL
created_at TIMESTAMPTZ NOT NULL DEFAULT now()
```

`row_id`'s type derives from `pk_sql_type_text(&m.pk_type)` — `BIGINT` for HeerId/HeerIdDesc/Serial, `UUID` for RanjId/RanjIdDesc, custom for `Custom`.

The synthesized table sorts after its source in the bucket's `BTreeMap<String, TableSchema>` (alphabetically — `_outbox` suffix sorts after the parent name).

**Note on PK function names** (per `project_heeranjid_pk_default_names.md`): use `heerid_next()` / `ranjid_next()` / `heerid_next_desc()` / `ranjid_next_desc()`. **NEVER** `generate_id_desc` / `generate_ranj_id*` — those don't exist in HeerRanjId 0.3.x and are the function-name drift that GH #132 (closed on 8ζ merge) tracks.

### 7.2 Decision: CHECK constraint on `action`

**Plan: do NOT add a `CHECK (action IN ('create','save','delete'))` constraint.**

Rationale: existing hand-written outbox fixtures (e.g. `tests/integration/migrations/phase4/005_notifications_outbox.sql`) do not have CHECK. Adding it via projection would break `sync_models` parity with these fixtures during the migration window. The `OutboxAction` enum guarantees correctness from djogi's emit_event side; CHECK is belt-and-suspenders only. If we want it, land as a separate cluster.

### 7.3 Tests

Add unit tests in `djogi/src/migrate/projection.rs`:

- `events_model_synthesises_outbox_with_bigint_row_id` — HeerId source.
- `events_heerid_desc_keyed_synthesises_outbox_with_bigint_row_id` — HeerIdDesc source.
- `events_ranjid_keyed_synthesises_outbox_with_uuid_row_id` — RanjId.
- `events_ranjid_desc_keyed_synthesises_outbox_with_uuid_row_id` — RanjIdDesc.
- `events_serial_keyed_synthesises_outbox_with_integer_row_id` — Serial.
- `non_events_model_does_not_synthesise_outbox` — gate test.
- `outbox_table_sorts_after_source_in_bucket_iter` — determinism.
- `events_model_with_app_label_outbox_lands_in_same_bucket` — app-bucket parity.

---

## 8. CI changes

### 8.1 Workflow surgery (PR 1 lays groundwork; PR 3 activates trybuild)

`.github/workflows/ci.yml`:

```yaml
- name: Format check
  run: cargo fmt --all -- --check

- name: Workspace clippy (compile-binding)
  run: cargo clippy --workspace --all-targets --features spatial,outbox,notify,testing -- -D warnings

- name: Validate JUSTIFICATION comments
  run: cargo xtask check-justifications

- name: Ordinary integration tests
  run: cargo test --workspace --features spatial,outbox,notify,testing -- --test-threads=1

- name: Pin suite
  run: cargo test -p djogi --tests pin/* --features raw_methods_for_pin_tests -- --test-threads=1

# PR 3 ONLY — activated when trybuild fixtures exist:
- name: Compile-fail trybuild gate
  run: cargo test -p djogi --test raw_sql_compile_fail --features testing
```

**Critical**: never run `cargo test --all-features` for ordinary tests — that would activate `raw_methods_for_pin_tests` if any feature wires it back in. The CI must enumerate features explicitly.

### 8.2 GHA minute budget

Cluster's existing CI runs ~20 PRs/month. Estimated added cost:
- xtask check-justifications: <5 sec (file walk + syn parse).
- Compile-fail trybuild (PR 3+): ~1 min warm-cache, ~3 min cold-cache.
- Pin suite: ~1 min (small, reuses Postgres service).
- Total added: <100 GHA min/month — well below 2k cap.

---

## 9. Verification gates (per-PR merge checklist)

### 9.1 PR 1 (additive harness) merge gates

```
[ ] cargo fmt --all -- --check
[ ] cargo clippy --workspace --all-targets --features spatial,outbox,notify,testing -- -D warnings
[ ] cargo build --workspace --all-targets --features spatial,outbox,notify,testing
[ ] DATABASE_URL=... cargo test --workspace --features spatial,outbox,notify,testing
[ ] cargo xtask check-justifications  # only the 9 pin-test JUSTIFICATIONs validated; tests/integration unchanged
[ ] cargo test -p djogi --tests pin/* --features raw_methods_for_pin_tests
[ ] CLAUDE.md section matches §3.7 (the `unsafe`-style framing)
[ ] docs/spec/raw-sql-escape-hatches.md exists and is the authoritative spec
[ ] docs/spec/decisions.md has the new decision row
[ ] CI workflow updated per §8.1 (xtask + pin suite steps live; trybuild step deferred)
[ ] GH #134 (projection synthesises outbox) tests pass
```

### 9.2 PR 2 (test refactor) merge gates

```
[ ] cargo fmt --all -- --check
[ ] cargo clippy --workspace --all-targets --features spatial,outbox,notify,testing -- -D warnings
[ ] DATABASE_URL=... cargo test --workspace --features spatial,outbox,notify,testing
[ ] cargo xtask check-justifications
[ ] git grep -E '\.raw_(query|rows|fetch_one|scalar|execute|ddl|stream)\(' tests/integration/    # zero results
[ ] git grep -E '\.with_client\(|\.pool\(\)|\.conn\(\)' tests/integration/                       # zero results
[ ] git grep 'tokio_postgres::' tests/integration/                                               # zero results
[ ] All 54 files refactored or relocated to tests/pin/ with attribute + JUSTIFICATION
[ ] phase5_zero_raw_in_atomic.rs moved to tests/pin/raw_execute_pin.rs (or equivalent)
```

### 9.3 PR 3 (lockdown) merge gates

```
[ ] cargo fmt --all -- --check
[ ] cargo clippy --workspace --all-targets --features spatial,outbox,notify,testing -- -D warnings
[ ] cargo build --workspace --all-targets --features spatial,outbox,notify,testing
[ ] DATABASE_URL=... cargo test --workspace --features spatial,outbox,notify,testing
[ ] cargo xtask check-justifications
[ ] cargo test -p djogi --test raw_sql_compile_fail --features testing  # trybuild — NEW IN PR 3
[ ] cargo test -p djogi --tests pin/* --features raw_methods_for_pin_tests
[ ] All 17 internal djogi modules compile with `use crate::__bypass::RawAccessExt;`
[ ] DjogiContext::raw_* are no longer pub inherent methods (verified by removed pub from context.rs)
[ ] DjogiContext::pool() and DjogiContext::conn() are pub(crate)
[ ] DjogiPool::with_client is pub(crate)
[ ] CI trybuild step is live (not deferred)
```

---

## 10. Branch strategy and 3-PR flow

### 10.1 Three-PR sequence (BLOCK-4 fix)

```
PR 1: harness/raw-methods-prevention-1-additive  →  main
        │
        │  (additive: trait + attribute + xtask + pin tests + spec docs)
        │  (raw_* still pub inherent — no compile breaks)
        │  (GH #134 outbox projection lands here)
        ▼
PR 2: harness/raw-methods-prevention-2-refactor  →  main
        │
        │  (54 tests refactored via 13 Sonnet batches)
        │  (raw_* still pub during this PR)
        │  (each batch is a separate commit; per-phase-prefix order)
        ▼
PR 3: harness/raw-methods-prevention-3-lockdown  →  main
        │
        │  (raw_* demoted to pub(crate) sealed extension trait)
        │  (17 internal modules add `use crate::__bypass::RawAccessExt;`)
        │  (trybuild compile-fail gate activates)
        ▼
Cluster 8ζ rebases on PR 3's main
```

Why 3 PRs:
- **Bisectable**: every PR is independently green; `git bisect` lands on a meaningful commit.
- **Reviewable**: PR 1 reviewer focuses on mechanism; PR 2 reviewer focuses on equivalence; PR 3 reviewer focuses on API-break completeness.
- **Reversible**: if PR 3 turns up unforeseen adopter breakage (unlikely — djogi is pre-publish), revert is one PR, not the whole stack.
- **Reviewer rotation**: Codex on PR 1 + PR 3 (mechanism + lockdown — novel); Sonnet drives PR 2 (mechanical); Codex adversarial-reviews each PR 2 batch.

### 10.2 PR 1 implementation flow

1. Codex implements PR 1 against this plan (mechanism + spec docs + xtask + pin tests + GH #134 sub-task).
2. Triple-review (Codex self-adversarial + Gemini + fresh Opus) — same `simplify-with-review` skill cadence.
3. Local verification: §9.1 gates.
4. PR opened against `main`.
5. CI green → merge.

### 10.3 PR 2 implementation flow

1. Branch `harness/raw-methods-prevention-2-refactor` off PR 1's merged main.
2. For each batch (1..13 per §6.3):
   a. Orchestrator (Claude) dispatches Sonnet subagent with batch file list + recipes.
   b. Sonnet refactors files, runs per-file tests, commits per file.
   c. Orchestrator runs §6.4 verification gate.
   d. Codex adversarial review on the batch's commits.
   e. Fix any BLOCKs.
3. Full PR 2 verification (§9.2 gates).
4. PR opened; merged.

### 10.4 PR 3 implementation flow + cluster 8ζ rebase

1. Branch `harness/raw-methods-prevention-3-lockdown` off PR 2's merged main.
2. Codex implements PR 3 against §1.3, §2.2, §2.3, §5.3, §5.5.
3. Triple-review.
4. Verification (§9.3 gates).
5. PR opened; merged.

**Cluster 8ζ rebase** (after PR 3 merges):

The 8ζ branch carries:
- Function-name sweep commit (`c0850c6`) — fixes `generate_id_desc` → `heerid_next_desc` etc. **Stays valid post-rebase.**
- CLAUDE.md additions (`d1b7fd1`, `9b198e3`, `497f6f8`, `b610e45`) — adds the "Tests must use djogi structs" guidance. **Conflicts with PR 1's CLAUDE.md rewrite (§3.7).** Resolution: drop the 8ζ CLAUDE.md commits during rebase; the harness's CLAUDE.md section supersedes them.
- `tests/integration/raw_methods_blacklist.rs` — runtime grep gate. **Now redundant; delete during rebase.**
- `PENDING_CLEANUP_133` allowlist constant in that file — also gone.
- 2 new tests using raw_* (`phase8_t11_notify_roundtrip.rs`, `phase8_t8_7_outbox_tombstones.rs`). **Refactor onto the typed surface as part of the rebase commit** — these are new files PR 2 didn't see.

**Conflict surface (be honest, BLOCK-2 fix):**
- `djogi/src/lib.rs` — PR 1 adds `pub mod __bypass;`. 8ζ adds `pub mod notify;` (T11). Both additive at module-list site: trivial three-way merge; accept both.
- `djogi/src/pg/pool.rs` — PR 3 demotes `with_client` to `pub(crate)`. 8ζ uses `pool.with_client` from `notify` integration code. Resolution: 8ζ's notify code (in `djogi/src/notify/`, internal) adds `use crate::__bypass::RawPoolAccessExt;` and calls `pool.raw_with_client(...)` instead — same internal-callers pattern as §5.3. One-line per call site.
- `Cargo.toml` (workspace + `djogi/Cargo.toml`) — PR 1 adds `[lints]` + xtask member; 8ζ adds `notify` optional dep. Both additive; trivial.
- `CLAUDE.md` — full conflict; PR 1 wins (drop 8ζ's section additions).

8ζ's rebase commit is therefore non-trivial but contained — the rebase is mostly a CLAUDE.md drop + a `with_client → raw_with_client` mechanical rename in `djogi/src/notify/` + a refactor of two new tests onto the typed surface. Estimate: 1–2 hours of careful work, not a redesign.

---

## 11. Risks and open questions

### 11.1 Resolved decisions

- **Attribute name**: `deliberately_bypass_convention_with_raw_sql`. See §0.0.
- **CHECK constraint on outbox `action`**: not added; see §7.2.
- **Allowlist mechanism**: none. The runtime blacklist (8ζ-local) is dropped at rebase.
- **Pool/conn demotion**: yes; `pub(crate)` with bypass attribute the only public unlock.
- **`atomic` reshape**: no — preserves `IntoAtomicScope` polymorphism. Pin tests reach pool via `RawPoolAccessExt::raw_pool`.
- **Internal-callers pattern**: explicit `use crate::__bypass::RawAccessExt;` per file; no JUSTIFICATION required for internal code.
- **3-PR split**: yes — additive → refactor → lockdown.
- **JUSTIFICATION format**: `(djogi#<n>)` for tests/integration; `(PIN)` for tests/pin. Adopters file on djogi's tracker, not their own.
- **trait-method shape**: `async fn` in trait + `#[trait_variant::make(_: Send)]` for the Send variant.
- **Macro on `mod foo;`**: explicit compile error.

### 11.2 Open

- **Q1 (counter-signal from Opus)**: would a value-marker shape (`RawSqlEscape<'_>` returned by an unlock fn that the bypass attribute brings into scope) be cleaner than the trait shape? Pro: more idiomatic Rust; can carry per-call metadata (issue number? span?). Con: requires every raw call to thread the marker; multi-call functions get verbose. Decision: **defer**. The trait shape is what v1 designed and what the proc macro injects via `use`. If we revisit later, it is a non-breaking refactor (the trait can be deprecated and replaced with the marker pattern in a future cluster).
- **Q2**: `trait_variant::make` is stable; verify it works with `async fn` returning a borrowed-self stream (`RawCursorStream<'a>`). If not, fall back to manual `impl Future` form for `raw_stream` and `raw_stream_with_fetch_size` only — these two methods would have hand-written returns, the other six would use `async fn`.
- **Q3**: rustc 1.75 MSRV — confirm djogi's MSRV is at least 1.75 (release of `async fn` in trait stable). If lower, raise MSRV in the same PR.
- **Q4**: xtask member in workspace — does adding a workspace member affect downstream `path = "../djogi"` consumers (e.g. sister crates)? It should not (xtask is a binary, not a library), but verify in CI.
- **Q5**: adopter API break — today an adopter's production code can write `ctx.raw_execute(...)`. After PR 3, they must add `#[djogi::deliberately_bypass_convention_with_raw_sql]` on the calling fn (or `use djogi::__bypass::RawAccessExt;` in their own non-test code). Per `project_djogi_prepublish.md`, djogi is pre-publish; the break is acceptable. Document in release notes.
- **Q6**: heeranjid sister-repo callers — does `heeranjid` ever invoke `raw_*` on a `DjogiContext`? Quick grep needed before PR 3 lands. (Almost certainly no — heeranjid is a leaf dep, not a djogi consumer.)

---

## 12. Implementation sequencing (detailed commit order per PR)

### 12.1 PR 1 — additive harness

**Phase A — Docs (the contract)**
1. `docs: add docs/spec/raw-sql-escape-hatches.md` (the canonical spec).
2. `docs: rewrite CLAUDE.md "Raw SQL is djogi's unsafe" section + ReadMe.MD note + decisions.md row`.
3. `docs(spec): pre-author rustdoc for djogi/src/__bypass.rs in docs/spec/internal/__bypass-rustdoc-draft.md`.

**Phase B — Mechanism (additive)**
4. `feat(macros): add deliberately_bypass_convention_with_raw_sql proc macro`. Body matches §1.2 + §3.3 (with explicit `mod foo;` error).
5. `feat(djogi): introduce __bypass::RawAccessExt + RawPoolAccessExt as additive sealed traits`. Module rustdoc copied from the docs draft. Existing `pub raw_*` inherent methods remain — the trait impl shadows them but they're still callable.
6. `feat(xtask): add cargo xtask check-justifications validator`.
7. `chore(lints): workspace clippy.toml disallowed_methods + per-crate overrides`.
8. `feat(migrate): synthesise {table}_outbox in projection (#134)`.

**Phase C — Pin tests (the carve-out)**
9. `test(pin): add pin tests for 8 raw APIs + raw_pool_access (9 files)`. All 9 carry the bypass attribute and `JUSTIFICATION (PIN)` comments.

**Phase D — CI**
10. `ci: explicit feature lists; add xtask + pin suite steps to ci.yml; trybuild step deferred to PR 3`.

### 12.2 PR 2 — test refactor

11. `test(integration): refactor phase1 tests off raw_*` (Sonnet batch 1).
12. `test(integration): refactor phase2 tests off raw_*` (Sonnet batch 2).
13. `test(integration): refactor phase3 tests off raw_*`.
14. `test(integration): refactor phase4 tests off raw_*`.
15. `test(integration): refactor phase5 tests off raw_*`.
16. `test(integration,pin): relocate phase5_zero_raw_in_atomic to tests/pin/`.
17. `test(integration): refactor phase6 tests off raw_*`.
18. `test(integration): refactor phase7 tests off raw_*`.
19. `test(integration): refactor phase7_5 tests off raw_*`.
20. `test(integration): refactor phase7_zero2 tests off raw_*`.
21. `test(integration): refactor phase8 compose+hooks+role tests off raw_*`.
22. `test(integration): refactor phase8 t7+t8 (Punnu) tests off raw_*`.
23. `test(integration): refactor phase8_zero tests off raw_*`.

(Codex adversarial-reviews each commit between batches; commits split further if any review surfaces a BLOCK that needs a separate fixup.)

### 12.3 PR 3 — lockdown

24. `refactor(djogi): add use crate::__bypass::RawAccessExt to 14 internal djogi/src modules` (no behaviour change).
25. `refactor(djogi-cli): add use djogi::__bypass::RawAccessExt to 3 djogi-cli/src modules`.
26. `feat(djogi): demote raw_* from inherent pub to RawAccessExt trait impl in __bypass.rs` (the API break — atomic commit).
27. `feat(djogi): demote pool() and conn() to pub(crate); RawPoolAccessExt::raw_pool / raw_conn unlock`.
28. `feat(djogi): demote DjogiPool::with_client to pub(crate); RawPoolAccessExt::raw_with_client unlock`.
29. `test(compile-fail): add trybuild fixtures asserting raw_* / pool / with_client / tokio_postgres direct don't resolve on bare DjogiContext`.
30. `ci: activate trybuild compile-fail gate in ci.yml`.

### 12.4 Discipline

Each commit is atomic, passes its own tests, and is bisectable. Every implementation commit (4–30) cites the spec section it enacts in its message body. The reviewer cycle (Codex + Gemini + Opus) checks contract adherence at every round — implementation that diverges from the docs without updating the docs first is a hard BLOCK.

---

## 13. What this plan does NOT cover (explicit non-goals)

- Adopter-side enforcement in their own repos (only a CLAUDE.md hint).
- A `djogi lint` CLI subcommand (the xtask is the validator).
- A separate `djogi-test-harness` crate for adopters.
- Forward-compat for an adopter who adds a new escape route by re-exporting `__bypass::RawAccessExt` from a wrapper crate (the trait is sealed, so re-export from outside djogi cannot create new impls).
- The notify watcher-died lifecycle gap (GH #131 — separate cluster).
- The `target/djogi_outbox/<table>_outbox.sql` build-time emission — runtime/projection-side synthesis is sufficient for this PR's scope.
- Refactoring `djogi/src/migrate/projection.rs` itself onto a different shape — this plan adds the outbox synthesis but doesn't restructure the existing projection code.

---

## End of plan

Reviewers: surface any gap that would let an ordinary integration test still reach `raw_*`, `pool()`, `conn()`, `with_client`, `batch_execute`, or `tokio_postgres::*` direct WITHOUT the bypass attribute. Surface any failure mode that would let the cluster-8ζ rebase break. Surface any ergonomic or maintainability concern that argues for a different mechanism. Specifically check whether the v1 BLOCK fixes are adequate (BLOCK-1: atomic preservation; BLOCK-2: rebase honesty; BLOCK-3: internal-callers enumeration; BLOCK-4: 3-PR split; BLOCK-5: xtask validator; BLOCK-6: baseline reset; BLOCK-7: async-fn-in-trait; BLOCK-8: explicit `mod foo;` error). Output: ALLOW / BLOCK with concrete findings.
