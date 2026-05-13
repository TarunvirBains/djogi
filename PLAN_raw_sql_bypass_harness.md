# Plan v3 — Raw SQL Escape Hatch Harness (GH #133 + GH #134)

> **Status: historical archive.** This plan shipped across PRs 1–3 and is
> retained for context only. References to `trybuild` in this document
> describe the original compile-fail driver under
> `tests/compile_fail/raw_sql.rs`; that driver was removed in Phase 8.5
> when lihaaf replaced trybuild as djogi's compile-fixture gate. The
> raw-SQL compile-fail fixtures now live at
> `djogi/tests/compile_fail/*.rs` with blessed lihaaf snapshots and run
> via `cargo lihaaf --manifest-path djogi/Cargo.toml` in CI. Do not use
> this document as a "use trybuild" instruction.

**Branch:** `harness/raw-methods-prevention` (off `origin/main` @ `8e007d2`).
**Worktree:** `/home/tarunvir/projects/djogi-harness/`.
**PR target:** `main`, **split into 3 PRs** (additive → refactor → lockdown).
**Cluster 8ζ disposition:** parked at `phase8-cluster-zeta-operational-tail`; rebases on PR 3's `main` after merge.

> **v3 changes from v2** (12 BLOCKs + counter-signal cleanup from Opus/Gemini/Codex):
> 1. `raw_stream` / `raw_stream_with_fetch_size` no longer tie `sql` and `params` lifetimes to the returned `RawCursorStream<'ctx>`; only `&mut self` is borrowed for the stream lifetime. (§1.1)
> 2. `trait-variant` is added as an explicit dependency; it is **not** assumed to arrive transitively. (§2.3, §12.1)
> 3. The bypass macro injects `#[allow(unused_imports)]` on its generated `use`s so decorated tests that only need one raw trait do not fail under `-D warnings`. (§3.3)
> 4. PR 1 cherry-picks / incorporates 8ζ commit `c0850c6` before GH #134 work so projection tests use the real HeeRanjID 0.3.x PK function names. (§7.0, §12.1)
> 5. Internal caller inventory now includes missed pool/direct-driver modules (`live_migrate/backfill.rs`, `migrate/reset.rs`) and separates raw-trait callers from clippy allow sites. (§5)
> 6. 8ζ rebase playbook no longer claims a `with_client → raw_with_client` notify rename; notify uses a dedicated `tokio_postgres::connect` and needs an explicit internal clippy allowance/helper because it polls `AsyncMessage`. (§10.4)
> 7. `async fn` + `trait_variant` is a compile-proven implementation step, not an assumption; if the stream methods fail the PR falls back to manual `impl Future` only for those two methods. (§1.1, §11.2)
> 8. JUSTIFICATION validation switches from ±3-line proximity to a syntactic attribute-stack walk over parsed items; comments are validated against the decorated item span and may be multi-line. (§1.6)
> 9. Trait split prose makes base-vs-Send variants visually distinct. (§1.1)
> 10. Success criterion 7 is softened: `__bypass` is public-but-hidden for deliberate adopter/sibling-crate opt-out, while this repo's `tests/` may not reference it directly. The xtask bans source-level `djogi::__bypass` references under `tests/`. (§0, §1.6, §5.4)
> 11. PR 1 adds the `notify` feature definition before CI references it; `raw_methods_for_pin_tests` is deliberately removed because pin tests already use explicit `--test` targets plus the bypass attribute, and an otherwise-empty feature is a latent bypass/confusion point. (§2.3, §8.1, §12.1)
> 12. `proc-macro2` gets the `span-locations` feature so the xtask can read parsed item/attribute spans. (§2.3, §12.1)
>
> v2's v1 fixes still stand: `IntoAtomicScope` polymorphism preserved, 3-PR split, baseline reset for 8ζ-only files, and explicit `mod foo;` rejection.

---

## 0. Goal and success criteria

**Goal:** make it structurally impossible for an ordinary integration test to bypass djogi's typed surface and reach raw SQL — at compile time, not at lint or runtime time.

**Success criteria** (every one must hold at PR 3 merge — earlier PRs hold subsets):

1. `cargo test --workspace` passes from a clean checkout against the standard local Postgres.
2. `cargo clippy --workspace --all-targets --features <explicit list> -- -D warnings` passes.
3. `cargo fmt --all -- --check` passes.
4. `cargo xtask check-justifications` passes (every `deliberately_bypass_convention_with_raw_sql` attribute under `tests/` is paired with a syntactically attached valid `JUSTIFICATION (djogi#<n>)` or `JUSTIFICATION (PIN)` comment; no loose proximity-only matches).
5. `cargo xtask check-test-surface` passes after the PR 2 refactor: **zero** ordinary workspace integration tests under `tests/integration/` or `djogi-cli/tests/integration/` reference any of `raw_query`, `raw_rows`, `raw_fetch_one`, `raw_scalar`, `raw_execute`, `raw_ddl`, `raw_stream`, `raw_stream_with_fetch_size`, `pool()`, `conn()`, `with_client`, `batch_execute`, `tokio_postgres::` direct, or source-level `djogi::__bypass`.
6. Every raw API has exactly one designated pin test under `tests/pin/`. Pin coverage matrix: 8 raw methods + pool/conn/with_client = at least 9 pin files (see §4).
7. The bypass attribute `#[djogi::deliberately_bypass_convention_with_raw_sql]` is the only blessed test-path unlock for this repository. `djogi::__bypass` remains public-but-hidden so sibling crates and adopters can consciously opt out, but direct source references to `djogi::__bypass` under `tests/` are banned by xtask.
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
| `// JUSTIFICATION (djogi#<n>): <reason>` | Filed as **djogi** GH issue #n — tracks the typed-surface gap upstream. | Adopter tests or a future explicitly quarantined non-ordinary test target; PR 2 ordinary `tests/integration/` must not keep these |
| `// JUSTIFICATION (PIN): exercises raw_<api> itself` | Pin test — the raw API IS what's being validated. | Only files under `tests/pin/` |

The comment is attached to the **decorated item**, not merely close in the file. `cargo xtask check-justifications` (§1.6) parses each test file, finds every decorated `fn` / `impl` / inline `mod`, and validates leading/trailing comment lines that belong to that item. Multi-line reasons are allowed when every continuation line is a `//` comment directly adjacent to the first JUSTIFICATION line:

```rust
#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#234): citext column needs case-insensitive equality;
// QuerySet cannot yet express LOWER(col) = LOWER($1). Remove this once
// djogi#234 lands.
async fn my_test(mut ctx: DjogiContext) {
    // ...
}
```

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
   - The workspace raw-callers pattern (§5) with explicit crate-local or `djogi::__bypass` alias imports.
   - The migration philosophy: "every reach for raw SQL walks through the verbose attribute and the justification. Friction is the design."
   - Pointers to `tests/pin/` and `tests/compile_fail/raw_sql/` as the canonical examples.

3. **`ReadMe.MD`** — public-facing project README. One-paragraph note that integration tests in this repository must use the typed surface; cross-link to `docs/spec/raw-sql-escape-hatches.md`.

4. **`docs/spec/decisions.md`** (existing) — append a numbered decision row recording: "Raw SQL is treated as djogi's `unsafe`. In this repository's tests, use of `raw_*` requires the `#[djogi::deliberately_bypass_convention_with_raw_sql]` attribute and a syntactically attached `// JUSTIFICATION (djogi#<n>):` or `(PIN)` comment. Source-level `djogi::__bypass` references under `tests/` are banned. Tracked at GH #133." Date the decision.

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
/// (in tests) or an explicit crate-local / `djogi::__bypass` alias import
/// (inside workspace raw-callers). The seal blocks foreign impls.
// Base trait: no Send bound. Kept for trait_variant's split output.
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
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<RawCursorStream<'a>, DjogiError>;

    async fn raw_stream_with_fetch_size<'a>(
        &'a mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
        fetch_size: u32,
    ) -> Result<RawCursorStream<'a>, DjogiError>;
}

// Send trait: generated by trait_variant and imported by the bypass macro.
// This is the trait ordinary call sites use.
impl sealed::Sealed for crate::DjogiContext {}
impl RawAccessExt for crate::DjogiContext { /* current bodies, moved verbatim */ }
```

**Why `async fn` + `trait_variant::make`** (replaces v1's `impl Future + Send + 'a` form):
- v1's hand-written `impl Future<Output = Result<RawCursorStream<'a>, _>> + Send + 'a` ties the future's bound, the borrow's lifetime, and the Send opt-in into one expression. For methods returning a self-borrowing stream (`RawCursorStream<'a>`), rustc's HRTB inference is fragile; in practice the hand-written form requires extra `for<'a>` bounds at every call site to compile.
- `async fn` in traits is stable as of Rust 1.75. Lifetime capture is automatic — the `'a` flows through correctly.
- `#[trait_variant::make(RawAccessExt: Send)]` produces both `RawAccessExtBase` (no Send bound) and `RawAccessExt: Send` variants. The `Send` variant is the public-facing trait; `Send` is required for djogi's tokio runtime.
- `trait-variant` is added explicitly in PR 1. It is **not** assumed to arrive through tokio or any transitive dependency.
- The stream signatures intentionally match current `DjogiContext::raw_stream` shape: the returned `RawCursorStream<'ctx>` borrows `&'ctx mut self`, but `sql` and `params` are only needed while declaring the cursor and are not stored in the stream. Do **not** tie `sql` / `params` to `'ctx`; that would over-constrain ordinary string/bind lifetimes for no runtime benefit.
- PR 1 includes explicit compile canaries for `raw_stream` and `raw_stream_with_fetch_size` through `RawAccessExt` inside `djogi/src/__bypass.rs`, using fully-qualified trait calls such as `<DjogiContext as RawAccessExt>::raw_stream(...)`. This is required because, while PR 1 leaves inherent `raw_*` methods public, ordinary method-call syntax resolves to the inherent methods and would not prove the trait path. If `trait_variant::make` cannot handle these self-borrowing async returns, the fallback is narrow: keep async trait methods for the six non-stream raw APIs and hand-write `impl Future + Send + 'ctx` returns only for the two stream methods.

```rust
#[cfg(test)]
#[allow(dead_code)]
async fn _raw_stream_trait_canary<'a>(
    ctx: &'a mut DjogiContext,
) -> Result<RawCursorStream<'a>, DjogiError> {
    let params: &[&(dyn ToSql + Sync)] = &[];
    <DjogiContext as RawAccessExt>::raw_stream(ctx, "SELECT 1", params).await
}
```

**Companion trait `RawPoolAccessExt`** — same shape, exposes pool-level raw access without breaking `IntoAtomicScope`:

```rust
// Base trait: pool-level unlocks. Send variant is generated as RawPoolAccessExt.
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
        F: for<'c> FnOnce(&'c mut tokio_postgres::Client) -> crate::pg::pool::ClientFuture<'c, R> + Send,
        R: Send + 'static;
}
```

The trait is `#[doc(hidden)]` (no rustdoc surface), the sealed marker is `pub(crate)` (no foreign impls possible), and the module is reachable as `djogi::__bypass::{RawAccessExt, RawPoolAccessExt}` for deliberate opt-out. Without the trait in scope, methods of those names do not resolve. Inside this repository's `tests/`, source-level references to `djogi::__bypass` are banned by xtask; the bypass attribute is the only accepted unlock.

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

**Attribute stacking rule.** When combined with `#[djogi::djogi_test]`, the bypass attribute must be the outermost attribute so it expands first and injects imports into the user's original async body before `djogi_test` rewrites that body into its inner wrapper:

```rust
#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (PIN): exercises raw_execute itself.
#[djogi::djogi_test]
async fn raw_execute_round_trips(mut ctx: djogi::DjogiContext) {
    // raw_* resolves here after bypass expands, then djogi_test wraps.
}
```

Do **not** put `#[djogi::djogi_test]` above the bypass attribute. `djogi_test` forwards unknown outer attributes to its generated wrapper, so the bypass imports would land in the wrapper block rather than the inner test body that contains the raw call.

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

**Demotion ships in PR 3, not PR 1.** PR 1 adds the harness additively (the trait + attribute exist; `raw_*` remains `pub`). PR 2 refactors ordinary workspace integration tests off raw/pool/direct-driver escapes. PR 3 flips `raw_*` to `pub(crate) sealed-trait-only` — that is when bare `ctx.raw_execute(...)` stops compiling. The split makes each PR bisectable and isolates the API-break to a single moment.

### 1.4 `transaction::atomic` — preserved polymorphism (NOT reshaped)

**v1 §1.4 was wrong.** It proposed reshaping `atomic` to take `&mut DjogiContext` and extract the pool internally. That collapses the existing `IntoAtomicScope` trait (sealed at `djogi/src/transaction.rs:75`), which today has TWO impls:

```rust
// djogi/src/transaction.rs:93
impl IntoAtomicScope for &DjogiPool { /* outermost — opens new tx */ }

// djogi/src/transaction.rs:144
impl IntoAtomicScope for &mut DjogiContext { /* nested — opens savepoint */ }
```

This polymorphism is load-bearing: outermost transaction opens a fresh BEGIN; nested calls open SAVEPOINTs. Reshaping `atomic` to a single signature destroys nested-savepoint dispatch.

**v3 plan: keep `atomic` exactly as-is.** Pin tests that need outermost transactions reach the pool through the bypass attribute:

```rust
#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (PIN): exercises raw_pool + atomic outermost-scope binding.
#[djogi::djogi_test]
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

### 1.5 Clippy `disallowed_methods` for residual escape routes (PR 3 activation)

The lint policy is specified in PR 1 docs, but the workspace `clippy.toml` file is **not** added until PR 3. If it lands in PR 1, the additive PR fails its own `cargo clippy --workspace --all-targets -- -D warnings` gate because ordinary integration tests and internal direct-driver substrate are intentionally still dirty. PR 3 activates the lint only after PR 2 has removed ordinary test references and in the same commit that adds the narrow internal/per-crate allows.

Workspace `clippy.toml` contents:

```toml
disallowed-methods = [
    { path = "tokio_postgres::Client::query", reason = "Use djogi's typed query/raw-bypass policy; ordinary tests must use Model::objects()" },
    { path = "tokio_postgres::Client::execute", reason = "Use djogi's typed mutation/raw-bypass policy; ordinary tests must use Model create/save/delete APIs" },
    { path = "tokio_postgres::Client::batch_execute", reason = "Use djogi's migration/test-sync helpers or an explicitly documented internal substrate allow" },
    { path = "tokio_postgres::Client::query_one", reason = "Same as `query`" },
    { path = "tokio_postgres::Client::query_opt", reason = "Same as `query`" },
    { path = "tokio_postgres::connect", reason = "Do not connect outside djogi's pool except in documented framework substrate code" },
    { path = "deadpool_postgres::Pool::get", reason = "Use djogi::transaction::atomic and the typed Model surface" },
]
```

Per-crate override in `djogi-cli/Cargo.toml`, `djogi-shell/Cargo.toml`, and selected internal functions for legitimate framework substrate usage. Examples: `djogi/src/migrate/reset.rs` owns database drop/create and must use `tokio_postgres::connect` against maintenance databases; 8ζ's `djogi/src/notify.rs` must use a dedicated `tokio_postgres::connect` because LISTEN consumers need the `Connection` half for `AsyncMessage` polling. See §5 for the full caller / allow-site split.

After PR 3 activation, CI runs `cargo clippy --workspace --all-targets -- -D warnings` so violations are build-failures.

### 1.6 `cargo xtask check-justifications` (replaces v1's grep gate)

v1 §0.1 line 125 proposed a grep gate (`git grep -E 'JUSTIFICATION \(#[0-9]+\)' | wc -l`). This is structurally weak (BLOCK-5):
- Cannot distinguish a JUSTIFICATION 30 lines away from one adjacent to the attribute.
- Cannot distinguish a JUSTIFICATION inside a string literal from a real comment.
- Cannot validate the parenthesised prefix shape.

v2 introduced a `cargo xtask` validator; v3 makes attachment syntactic and stages broad source-surface checks so PR 1 stays additive:

```rust
// xtask/src/check_justifications.rs

/// Validates that every `#[djogi::deliberately_bypass_convention_with_raw_sql]`
/// attribute under `tests/` is paired with a valid JUSTIFICATION comment
/// syntactically attached to the same decorated item.
///
/// Discovery: walk every `.rs` file under `tests/` (skipping `target/`).
/// Use `syn::parse_file` to find every `Attribute` whose path resolves to
/// the bypass attribute. Record the decorated item span and attribute span.
///
/// Validation: for each decorated item, read the source file and inspect the
/// contiguous source lines belonging to that item's attribute stack and
/// signature prelude (from the first outer attribute/comment through the
/// item signature line). Find a JUSTIFICATION line attached to that stack,
/// not merely nearby text.
///
/// Reject:
///   - a JUSTIFICATION comment separated from the attribute stack by a blank
///     line or non-comment item,
///   - a JUSTIFICATION inside a string literal or function body,
///   - any source-level `djogi::__bypass` / `::djogi::__bypass` reference
///     under tests/ (manual bypass import or fully-qualified call).
///
/// Accepted JUSTIFICATION forms:
///   - `// JUSTIFICATION (djogi#<digits>): <reason>`  (any non-pin test)
///   - `// JUSTIFICATION (PIN): exercises raw_<api> itself` (pin tests only)
/// Continuation lines are allowed when they are immediately adjacent `//`
/// comments after the first JUSTIFICATION line. The first line must still
/// contain a non-empty reason after `): `.
///
/// Plain-English grammar (no Rust regex per project policy):
///   1. Strip leading whitespace.
///   2. Match literal `// JUSTIFICATION (`.
///   3. Either: literal `djogi#` + 1+ ASCII digits + literal `): ` + non-empty reason
///      Or:    literal `PIN): ` + non-empty reason (only allowed for files under tests/pin/)
///   4. Continuation lines, if present, must strip to `// <non-empty text>`.
///   5. Reject anything else.
///
/// Output: zero exit code on success; non-zero with a per-violation report
/// on failure. The error message for a malformed JUSTIFICATION is the
/// adopter-side filing rule from §0.1.
pub fn run() -> ExitCode { ... }
```

**Implementation approach** (Codex chooses the exact form, but must satisfy):
- `syn` for item + attribute discovery — never miss an attribute regardless of indentation or multi-line attribute lists.
- `proc-macro2` with the `span-locations` feature for item/attribute line spans. This is a PR 1 dependency change; do not rely on default `proc-macro2` spans.
- Source-file line walk for comment attachment — the syn AST does not carry comments, so the validator reads the raw file and reconstructs the contiguous attribute/comment stack for the decorated item.
- Treat `///` doc comments and multi-line attribute arguments as part of the stack for adjacency purposes, but only ordinary `// JUSTIFICATION ...` comments satisfy the grammar. `cfg_attr(..., djogi::deliberately_bypass_convention_with_raw_sql)` is rejected in tests: the bypass must be a concrete outer attribute so the validator and human reviewers see it.
- A direct source scan for forbidden manual bypass references under `tests/`: reject `djogi::__bypass` and `::djogi::__bypass` in source text. The proc macro's generated imports do not appear in source files, so this only catches human-written bypasses.
- The grammar is enforced via byte-level checks (`u8::is_ascii_digit`, slice equality) per project no-regex policy. Spell out the rule in plain English in the rustdoc.

### 1.7 `cargo xtask check-test-surface` (PR 2+ lockdown scan)

`check-justifications` must pass in PR 1 while `tests/integration/` is intentionally still dirty, so the broad "ordinary tests use no raw surface" scan is a separate command first required by the PR 2 merge gate and CI after the refactor.

`cargo xtask check-test-surface` walks `.rs` files under every workspace integration-test root that exists today:

```text
tests/integration/
djogi-cli/tests/integration/
```

It ignores comments/string literals and rejects ordinary integration-test code references to:

```text
raw_query
raw_rows
raw_fetch_one
raw_scalar
raw_execute
raw_ddl
raw_stream
raw_stream_with_fetch_size
.pool(
.conn(
.with_client(
batch_execute
tokio_postgres::
djogi::__bypass
::djogi::__bypass
```

This command is intentionally source-policy based. Direct `tokio_postgres::` use is not a rustc name-resolution failure, and `tokio_postgres::Client::batch_execute` belongs to an external crate, so trybuild cannot make those APIs disappear. PR 2 may use equivalent `git grep` checks during the refactor, but the durable CI gate is the xtask command.

The command also supports `--list`, which prints the distinct violating file paths without failing on the first violation. PR 2 uses that output as the dispatch inventory.

**Scope limitation.** `check-justifications` validates bypass attributes under test roots; `check-test-surface` scans ordinary workspace integration-test roots named in §1.7. Workspace raw callers outside tests (§5) use the trait via crate-local or `djogi::__bypass` alias imports directly — they are framework/internal-example code and do not require JUSTIFICATION comments. The xtask does not flag them.

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
xtask/src/check_test_surface.rs               # PR 2+ lockdown scan from §1.7
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
tests/compile_fail/raw_sql_compile_fail.rs    # trybuild driver
clippy.toml                                   # workspace-level disallowed_methods config; lands with internal/per-crate allows
```

These ship in PR 3 because they assert non-resolution of djogi-owned methods — only true after the demotion lands. Direct `tokio_postgres::` use is not a rustc name-resolution failure; it is enforced by PR 3's clippy `disallowed_methods` activation plus `cargo xtask check-test-surface`, not trybuild.

### 2.3 Modified files

**PR 1** (additive):
```
djogi/src/lib.rs                              # pub mod __bypass; (additive)
djogi-macros/src/lib.rs                       # export deliberately_bypass_convention_with_raw_sql
djogi/Cargo.toml                              # new [[test]] entries for tests/pin/* + notify feature definition + trait-variant.workspace dep
Cargo.toml (workspace root)                   # xtask member + trait-variant workspace dep + proc-macro2(span-locations)
CLAUDE.md                                     # rule rewrite (§3.7)
ReadMe.MD                                     # one-paragraph harness note
docs/spec/decisions.md                        # new decision row
.github/workflows/ci.yml                      # add xtask steps (test-surface deferred until PR 2) + trybuild step (deferred until PR 3)
```

**PR 2** (test refactor; no production library API changes):
```
tests/integration/phase{1..8}_*.rs            # root integration files refactored; raw/pool/direct-driver call sites removed
djogi-cli/tests/integration/*.rs              # CLI integration files refactored or explicitly routed; same zero-raw ordinary-test rule
djogi/src/pg/pool.rs                          # only if pool lifecycle assertions are moved into internal unit tests
```

**PR 3** (lockdown):
```
djogi/src/context.rs                          # raw_* methods demoted; impl moved to __bypass.rs
djogi/src/pg/pool.rs                          # with_client demoted; RawPoolAccessExt impl
djogi/src/{query/refresh,testing,migrate/*,live_migrate/*,outbox/publishers/notify}.rs
                                               # internal raw callers add RawAccessExt alias imports
djogi/src/{live_migrate/backfill.rs,migrate/reset.rs}
                                               # pool/direct-driver escape allow-sites; see §5
Cargo.toml (workspace root)                   # workspace.lints.clippy + clippy config activation
djogi-cli/src/{analyze,live,verify}.rs        # internal callers add RawAccessExt alias imports
examples/elephant-tracker/src/{main,migrate,seed,visages/herd_summary,demos/*}.rs
                                               # workspace example raw callers import public-hidden bypass or refactor typed
djogi-cli/Cargo.toml                          # [lints.clippy.disallowed-methods] = "allow"
djogi-shell/Cargo.toml                        # same
.github/workflows/ci.yml                      # trybuild gate goes live
```

### 2.4 Files NOT touched on this branch

- `tests/integration/raw_methods_blacklist.rs` — does not exist on `origin/main`. It exists on cluster 8ζ (`phase8-cluster-zeta-operational-tail`). When 8ζ rebases on PR 3's main, 8ζ's rebase commit removes the file (the harness supersedes it). See §10.4.

- `PENDING_CLEANUP_133` allowlist constant — same disposition; lives in 8ζ's `raw_methods_blacklist.rs`, removed at 8ζ's rebase.

`tests/integration/phase5_zero_raw_in_atomic.rs` is **moved** to `tests/pin/raw_execute_pin.rs` in PR 2. Any scalar-specific coverage from that fixture lands in `tests/pin/raw_scalar_pin.rs`. Both files are decorated with `#[djogi::deliberately_bypass_convention_with_raw_sql]` and given `JUSTIFICATION (PIN)` comments.

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
    let injected_access: syn::Stmt = parse_quote!(
        #[allow(unused_imports)]
        use ::djogi::__bypass::RawAccessExt;
    );
    let injected_pool: syn::Stmt = parse_quote!(
        #[allow(unused_imports)]
        use ::djogi::__bypass::RawPoolAccessExt;
    );

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
            let access_use: syn::Item = parse_quote!(
                #[allow(unused_imports)]
                use ::djogi::__bypass::RawAccessExt;
            );
            let pool_use: syn::Item = parse_quote!(
                #[allow(unused_imports)]
                use ::djogi::__bypass::RawPoolAccessExt;
            );
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

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (PIN): exercises raw_execute round-trip; this IS the API being validated.
#[djogi::djogi_test]
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

### 3.6 `clippy.toml` (workspace root, PR 3)

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
syntactically attached to the decorated item, validated by
`cargo xtask check-justifications`. The
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
This repository's tests may not manually reference `djogi::__bypass`; use
the bypass attribute so the use site stays auditable.

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

The harness must not break djogi's own crates. v3 splits the inventory into three distinct surfaces because they need different treatments:

1. `raw_*` methods on `DjogiContext` — add `RawAccessExt` imports when the methods move to the trait.
2. pool/connection escape routes (`pool()`, `conn()`, `with_client`) — add `RawPoolAccessExt` imports or use internal helpers.
3. direct `tokio_postgres` / `batch_execute` substrate — keep internal clippy allowances narrow and explicit.

### 5.1 RawAccessExt callers in `djogi/src/`

```
djogi/src/context.rs                   # trait impl source — special case, lives in __bypass.rs after PR 3
djogi/src/live_migrate/daemon.rs       # raw_rows + raw_execute for live migration apply
djogi/src/live_migrate/state.rs        # raw_ddl for live state install
djogi/src/migrate/audit.rs             # raw_ddl for audit table writes
djogi/src/migrate/ledger.rs            # raw_ddl for ledger install
djogi/src/migrate/repair.rs            # raw_ddl for schema repair
djogi/src/migrate/runner.rs            # raw_ddl for migration apply/rollback
djogi/src/migrate/seed.rs              # raw_execute + raw_ddl for seed ledger/runs
djogi/src/outbox/publishers/notify.rs  # raw_execute for NOTIFY emission
djogi/src/query/refresh.rs             # raw_query::<T> for materialized refresh select-back
djogi/src/testing.rs                   # raw_ddl for sync_models/test scaffolding
```

Doc-only mentions in files such as `descriptor.rs`, `expr/mod.rs`, and `query/field.rs` are not raw callers and do not need imports.

### 5.2 RawAccessExt callers in sibling workspace crates

```
djogi-cli/src/analyze.rs   # raw_rows for schema analysis
djogi-cli/src/live.rs      # raw_rows for live commands
djogi-cli/src/verify.rs    # raw_rows for verification
```

The workspace root includes `examples/elephant-tracker` as a member, so PR 3 must also cover its deliberate example-level raw SQL and pool-level escape use. Either refactor to typed APIs where the example is not specifically demonstrating raw SQL, or add explicit `use djogi::__bypass::{RawAccessExt as DjogiRawAccessExt, RawPoolAccessExt as DjogiRawPoolAccessExt};` imports in:

```
examples/elephant-tracker/src/migrate.rs
examples/elephant-tracker/src/seed.rs
examples/elephant-tracker/src/main.rs
examples/elephant-tracker/src/visages/herd_summary.rs
examples/elephant-tracker/src/demos/cross_border_herds.rs
examples/elephant-tracker/src/demos/lineage.rs
examples/elephant-tracker/src/demos/mating_pairs.rs
```

At current `main`, `examples/elephant-tracker/src/migrate.rs` uses both `ctx.pool()` and `pool.with_client`, `examples/elephant-tracker/src/seed.rs` uses `ctx.pool()`, and `examples/elephant-tracker/src/main.rs` uses a raw `batch_execute` on a pooled client. PR 3 must route those through `RawPoolAccessExt` or an example-local typed helper, not just add `RawAccessExt`. These example imports are not ordinary integration-test bypasses and are outside xtask JUSTIFICATION validation, but they should be mentioned in example docs/release notes because they demonstrate the public opt-out path.

### 5.3 Pool/direct-driver escape sites

These are **not** fixed by `RawAccessExt` alone:

```
djogi/src/live_migrate/backfill.rs   # ctx.pool() at current line 415; after pool() demotion use RawPoolAccessExt::raw_pool or an internal helper
djogi/src/migrate/reset.rs           # tokio_postgres::connect against maintenance DBs; narrow #[allow(clippy::disallowed_methods)]
djogi/src/testing.rs                 # tokio_postgres::connect + batch_execute for per-test DB create/drop; narrow allow
djogi/src/migrate/bootstrap.rs       # GenericClient::batch_execute for phase-zero bootstrap; narrow allow
djogi/src/pg/{connection,cursor,pool}.rs
                                     # lowest-level driver wrappers; narrow allow at wrapper functions/tests
djogi/src/transaction.rs             # internal BEGIN/SAVEPOINT/ROLLBACK batch_execute on PgConnection
```

8ζ adds:

```
djogi/src/notify.rs                  # tokio_postgres::connect for LISTEN/AsyncMessage polling; cannot be replaced by raw_with_client
```

### 5.4 Internal-callers pattern (PR 3)

Internal callers do **not** use the `#[djogi::deliberately_bypass_convention_with_raw_sql]` attribute. The attribute is a discoverability aid for tests and adopters; for crate-internal code, a plain `use` is cleaner:

```rust
// djogi/src/migrate/runner.rs (and sibling raw_* callers in djogi/src/)

use crate::__bypass::RawAccessExt as DjogiRawAccessExt;
// (and `use crate::__bypass::RawPoolAccessExt;` if pool-level access is used)

// raw_execute, raw_query, etc. now resolve as before.
```

```rust
// djogi-cli/src/analyze.rs (and 2 sibling modules)

use djogi::__bypass::RawAccessExt as DjogiRawAccessExt;

// raw_query etc. now resolve as before.
```

```rust
// examples/elephant-tracker/src/migrate.rs (and sibling example modules)

use djogi::__bypass::{
    RawAccessExt as DjogiRawAccessExt,
    RawPoolAccessExt as DjogiRawPoolAccessExt,
};

// Example-level raw SQL remains deliberate and visible.
```

`RawPoolAccessExt` is implemented for both `DjogiContext` and `DjogiPool`: on `DjogiContext`, `raw_pool` / `raw_conn` expose the backing context internals and `raw_with_client` delegates through the backing pool; on `DjogiPool`, `raw_pool` returns `Some(self)`, `raw_conn` returns `None`, and `raw_with_client` delegates to the demoted inherent `with_client`. That makes current `ctx.pool()?.with_client(...)` call sites mechanically rewriteable to `ctx.raw_pool()?.raw_with_client(...)`.

The alias is optional but preferred: it avoids collisions if a module already has a local `RawAccessExt` symbol. Trait method resolution works through an imported alias. The `__bypass` module is `pub` (so sibling djogi crates and deliberate adopter opt-outs can import it) but `#[doc(hidden)]` (so it doesn't surface in rustdoc). The seal blocks foreign impls — adopters of djogi cannot create their own `RawAccessExt`-implementing types.

### 5.5 Internal-callers don't need JUSTIFICATION

`check-justifications` validates bypass attributes only under test roots. Internal `djogi/src/`, `djogi-cli/src/`, and example source files are exempt from JUSTIFICATION-comment validation by design — they are framework-internal or deliberate example code, and the audit log is not the right surface for djogi's own substrate use.

This decision is reversible: a future xtask flag could scan internal files too if internal raw SQL becomes a code-review concern. For PR 3, the limit is workspace integration-test roots plus bypass-attribute validation.

### 5.6 Internal-callers ship in PR 3

The raw-call modules add their `use` statements in the same commit that demotes `raw_*` from inherent `pub` to sealed extension trait. Do **not** land an import-only commit while inherent methods still exist: inherent methods win method resolution, the trait imports are unused, and `-D warnings` would make that commit fail. Direct-driver clippy allow sites land with the workspace lint activation. Within PR 3, the commit order:

1. Activate workspace `clippy.toml` and add narrow clippy allows for internal direct-driver functions (`migrate/reset.rs`, `testing.rs`, `migrate/bootstrap.rs`, `notify.rs` after 8ζ rebase, and low-level pg/transaction wrappers) plus per-crate overrides for CLI/shell.
2. Move the `raw_*` bodies from inherent `impl DjogiContext` to `impl RawAccessExt for DjogiContext` in `__bypass.rs`; demote inherent methods to `pub(crate)` or remove their public inherent surface; in the same commit, add `RawAccessExt` alias imports to every raw-call module in §5.1/§5.2.
3. Demote `pool()`, `conn()`, `DjogiPool::with_client` to `pub(crate)` and update `live_migrate/backfill.rs`, pool-level examples, and any remaining workspace pool callers to use `RawPoolAccessExt` or crate-internal helpers.
4. Land the trybuild compile-fail fixtures.

The order makes step 2 the atomic raw-API break — bisectable to a single hash and green under `-D warnings`.

---

## 6. Migration: ordinary workspace integration tests (PR 2)

### 6.1 Phase order (oldest first)

phase1 → phase2 → phase3 → phase4 → phase4_5 → phase5 → phase5_5 → phase5_zero → phase6 → phase6_5 → phase7 → phase7_5 → phase7_zero → phase7_zero2 → phase8 → phase8_zero

### 6.2 Per-test playbook

Scope: every ordinary integration test under `tests/integration/` and `djogi-cli/tests/integration/`. Before PR 2 starts, generate the authoritative violating-file inventory with `cargo xtask check-test-surface --list` (or equivalent dry-run output from the same scanner). The batch table below is a dispatch scaffold, not a substitute for the generated inventory.

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
   - `pool().clone()` + `atomic(&pool, ...)` → `atomic(&mut ctx, ...)` if no pool-level need; if the pool-level API itself is being validated, move only that minimal assertion to `tests/pin/raw_pool_access_pin.rs`; otherwise surface a typed-surface gap.
3. If the test uses `#[model(events)]`: the `_outbox` table must be projected by `sync_models` — depends on §7 (GH #134 sub-task), which lands in PR 1.
4. If the typed surface genuinely cannot express the test's need, **stop and surface the gap**: file a djogi GH issue and decide explicitly whether PR 2 grows the typed surface or defers/removes that assertion. Do **not** move a typed-surface gap into `tests/pin/`; pin tests are only for validating the raw APIs themselves and use `JUSTIFICATION (PIN)`. Do **not** leave a non-pin raw escape in ordinary workspace integration tests: `cargo xtask check-test-surface` has no skip directive and must remain a zero-raw gate.
5. Run the single test locally: `DATABASE_URL=postgres://djogi:djogi@localhost:5432/djogi_test cargo test -p djogi --test <name>`.
6. Commit the refactor. Per the atomic-commits memory, prefer one commit per test file; per phase batch is acceptable when the changes are mechanical and co-located.

### 6.3 Sonnet/Spark batching (the dispatch model)

PR 2 is mechanical pattern-matching at scale. Codex `--effort xhigh` is the wrong tool — too expensive, too slow for repetitive work. Sonnet can own whole phase batches; GPT-5.3 Codex Spark should receive **one test file at a time** unless the files are mechanically coupled by fixtures or a move to `tests/pin/`.

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
| 14 | phase8 CLI/pool stragglers | `phase8_djogi_verify_cli.rs`, `phase8_on_commit_pool_warn.rs`, `phase8_zero_pool_live.rs`, `phase8_zero_pool_bench.rs`, `djogi-cli/tests/integration/phase8_djogi_analyze_recommendations.rs` | CLI/database reset + pool lifecycle tests need explicit typed/pin/internal routing |

Any file emitted by `cargo xtask check-test-surface --list` and absent from this table becomes its own Spark packet before PR 2 can merge.

Each Sonnet/Spark subagent receives:
- The harness API surface (attribute + `djogi_test(sync_models=[...])`).
- File list for its batch, or exactly one test file for Spark.
- Canonical refactor recipes from §6.2.
- Instruction: when the typed API genuinely doesn't exist, stop and surface the gap to the orchestrator (Claude), who files the djogi GH issue and decides whether to extend the typed surface in PR 2 or defer the test. The subagent does not move gap tests into `tests/pin/`.
- Per-file commit (atomic-commits memory).
- `cargo test --test <test_name>` must pass before handoff.

**Spark work-packet template.** The orchestrator should dispatch Spark with this exact shape so the work is independently executable:

```text
Task: refactor one ordinary integration test off raw SQL.
Target file: tests/integration/<file>.rs or djogi-cli/tests/integration/<file>.rs
Write scope: this target file only, unless the packet explicitly lists sibling fixtures/snapshots under tests/integration/migrations/<phase>/ or djogi-cli test fixtures that the target file owns.
Allowed exception: if the target is phase5_zero_raw_in_atomic.rs, move only the raw-API pin coverage to tests/pin/raw_execute_pin.rs and tests/pin/raw_scalar_pin.rs as described in §4/§6.2; if the target validates pool/with_client lifecycle itself, move only the minimal raw-pool API assertion to tests/pin/raw_pool_access_pin.rs or convert the test to an internal pg/pool unit test.
Read first: the target file end-to-end, the model definitions it uses, and any fixtures referenced by path.
Required edits: remove ordinary raw_*/pool()/conn()/with_client/batch_execute/tokio_postgres:: use by replacing with typed djogi APIs from §6.2.
Fixture coupling: if the refactor changes a migration fixture, snapshot.json, or hand-written SQL file, stop unless that sibling file is in the packet's write scope; report the exact fixture path and why it must change.
Stop condition: if the typed API cannot express the assertion, do not invent a raw escape; report the missing typed surface and the smallest failing assertion to the orchestrator.
Verification: run the single test command from §6.2 if a database is available; otherwise report that it was not run.
Handoff: list changed files, raw/pool/direct-driver call sites removed, remaining gaps, and the exact verification command/output.
```

Spark must not edit harness mechanism files, `Cargo.toml`, `CLAUDE.md`, clippy config, or production internal djogi source during PR 2. If a pool lifecycle integration test must become an internal `djogi/src/pg/pool.rs` unit test, the packet stops and the orchestrator/Codex performs that targeted test move.

Codex stays reserved for: PR 1 (novel mechanism), PR 3 (lockdown semantics + internal-callers sweep), and adversarial review on every Sonnet/Spark batch's output.

### 6.4 Verification gate per phase batch

After each Sonnet/Spark batch's refactor, the orchestrator runs:

- `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings` — must pass.
- `DATABASE_URL=... cargo test --workspace` — must pass.
- `cargo xtask check-test-surface --list` — must shrink each batch (canonical scanner; ignores comments/string literals).
- `cargo xtask check-justifications` — must pass (any new pin-tests added during the batch have valid JUSTIFICATIONs).

### 6.5 Events-bearing tests blocker (resolved by PR 1's GH #134 sub-task)

Tests using `#[model(events)]` cannot refactor onto `#[djogi_test(sync_models=[Model])]` until projection synthesizes the `{table}_outbox` companion (GH #134). This sub-task lands in **PR 1** (§7 below). PR 2 starts only after PR 1 merges, so all events-bearing tests are unblocked from batch 1.

Files affected: identify with `git grep -lE '#\[model[^]]*\bevents\b' tests/integration/`.

---

## 7. Sub-task: GH #134 — projection synthesizes `{table}_outbox` (PR 1)

This sub-task ships in **PR 1** (the additive harness PR) because it is a precondition for refactoring events-bearing tests in PR 2.

### 7.0 Baseline prerequisite: HeeRanjID function-name sweep

Before implementing §7.1, PR 1 cherry-picks or otherwise incorporates 8ζ commit `c0850c6` (the function-name sweep: `generate_id_desc` → `heerid_next_desc`, `generate_ranj_id*` → `ranjid_next*`, etc.). Without that baseline, the new projection tests would either encode obsolete PK defaults or conflict with 8ζ during rebase.

Do not treat the cherry-pick as mechanical. Before landing it, inspect the commit diff and verify whether it depends on a HeerRanjID version bump, migration fixture rewrites, or sibling snapshot changes from 8ζ. If the commit touches fixtures under `tests/integration/migrations/` or assumes a dependency version not already on `main`, include those companion changes in the same PR 1 baseline commit or split the projection work until the baseline is coherent. Land this audited baseline before the GH #134 projection commit.

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

### 8.1 Workflow surgery (PR 1 lays groundwork; PR 2/PR 3 activate lockdown gates)

`.github/workflows/ci.yml`:

```yaml
- name: Format check
  run: cargo fmt --all -- --check

- name: Workspace clippy (compile-binding)
  run: cargo clippy --workspace --all-targets --features spatial,outbox,notify,testing -- -D warnings

- name: Validate JUSTIFICATION comments
  run: cargo xtask check-justifications

# PR 2 final commit ONLY — activated after all workspace integration-test roots are refactored:
- name: Validate ordinary test surface
  run: cargo xtask check-test-surface

- name: Ordinary integration tests
  run: cargo test --workspace --features spatial,outbox,notify,testing -- --test-threads=1

- name: Pin suite
  run: >
    cargo test -p djogi --features testing
    --test raw_execute_pin
    --test raw_query_pin
    --test raw_rows_pin
    --test raw_fetch_one_pin
    --test raw_scalar_pin
    --test raw_ddl_pin
    --test raw_stream_pin
    --test raw_stream_with_fetch_size_pin
    --test raw_pool_access_pin
    -- --test-threads=1

# PR 3 ONLY — activated when trybuild fixtures exist:
- name: Compile-fail trybuild gate
  run: cargo test -p djogi --test raw_sql_compile_fail --features testing
```

**Critical**: never run `cargo test --all-features` for ordinary tests — the CI must enumerate features explicitly so raw-surface harness behaviour never changes under a broad feature flag.

**Feature definitions added in PR 1 before CI references them:**

```toml
# djogi/Cargo.toml
notify = []

```

There is deliberately no `raw_methods_for_pin_tests` feature. Pin tests are selected by explicit `--test` target names and use the same bypass attribute path as any other explicit raw escape.

### 8.2 GHA minute budget

Cluster's existing CI runs ~20 PRs/month. Estimated added cost:
- xtask check-justifications + check-test-surface: <5 sec each (file walk + syn parse / source scan).
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
[ ] cargo test -p djogi --features testing --test raw_execute_pin --test raw_query_pin --test raw_rows_pin --test raw_fetch_one_pin --test raw_scalar_pin --test raw_ddl_pin --test raw_stream_pin --test raw_stream_with_fetch_size_pin --test raw_pool_access_pin
[ ] CLAUDE.md section matches §3.7 (the `unsafe`-style framing)
[ ] docs/spec/raw-sql-escape-hatches.md exists and is the authoritative spec
[ ] docs/spec/decisions.md has the new decision row
[ ] CI workflow updated per §8.1 (check-justifications + pin suite live; check-test-surface deferred to final PR 2 commit; trybuild deferred to PR 3)
[ ] GH #134 (projection synthesises outbox) tests pass
```

### 9.2 PR 2 (test refactor) merge gates

```
[ ] cargo fmt --all -- --check
[ ] cargo clippy --workspace --all-targets --features spatial,outbox,notify,testing -- -D warnings
[ ] DATABASE_URL=... cargo test --workspace --features spatial,outbox,notify,testing
[ ] cargo xtask check-justifications
[ ] cargo xtask check-test-surface
[ ] cargo xtask check-test-surface --list                                                        # no output
[ ] All files listed by `cargo xtask check-test-surface --list` are refactored or explicitly routed to pin/internal tests
[ ] phase5_zero_raw_in_atomic.rs moved to tests/pin/raw_execute_pin.rs (or equivalent)
```

### 9.3 PR 3 (lockdown) merge gates

```
[ ] cargo fmt --all -- --check
[ ] cargo clippy --workspace --all-targets --features spatial,outbox,notify,testing -- -D warnings
[ ] cargo build --workspace --all-targets --features spatial,outbox,notify,testing
[ ] DATABASE_URL=... cargo test --workspace --features spatial,outbox,notify,testing
[ ] cargo xtask check-justifications
[ ] cargo xtask check-test-surface
[ ] cargo test -p djogi --test raw_sql_compile_fail --features testing  # trybuild — NEW IN PR 3
[ ] cargo test -p djogi --features testing --test raw_execute_pin --test raw_query_pin --test raw_rows_pin --test raw_fetch_one_pin --test raw_scalar_pin --test raw_ddl_pin --test raw_stream_pin --test raw_stream_with_fetch_size_pin --test raw_pool_access_pin
[ ] All workspace raw/pool-call modules compile: `djogi/src` uses crate-local bypass aliases; `djogi-cli` and `examples/elephant-tracker` use `djogi::__bypass::{RawAccessExt, RawPoolAccessExt}` aliases as needed
[ ] Internal direct-driver allow sites are narrow and documented (`testing.rs`, `migrate/reset.rs`, `migrate/bootstrap.rs`, low-level pg/transaction wrappers; `notify.rs` after 8ζ)
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
        │  (ordinary workspace integration tests refactored via phase batches or Spark per-file packets)
        │  (raw_* still pub during this PR)
        │  (each batch is a separate commit; per-phase-prefix order)
        ▼
PR 3: harness/raw-methods-prevention-3-lockdown  →  main
        │
        │  (raw_* demoted to pub(crate) sealed extension trait)
        │  (workspace raw-call modules add crate-local or public-hidden RawAccessExt alias imports)
        │  (trybuild compile-fail gate activates)
        ▼
Cluster 8ζ rebases on PR 3's main
```

Why 3 PRs:
- **Bisectable**: every PR is independently green; `git bisect` lands on a meaningful commit.
- **Reviewable**: PR 1 reviewer focuses on mechanism; PR 2 reviewer focuses on equivalence; PR 3 reviewer focuses on API-break completeness.
- **Reversible**: if PR 3 turns up unforeseen adopter breakage (unlikely — djogi is pre-publish), revert is one PR, not the whole stack.
- **Reviewer rotation**: Codex on PR 1 + PR 3 (mechanism + lockdown — novel); Sonnet/Spark drives PR 2 (mechanical); Codex adversarial-reviews each PR 2 batch or Spark packet.

### 10.2 PR 1 implementation flow

1. Codex implements PR 1 against this plan (mechanism + spec docs + xtask + pin tests + GH #134 sub-task).
2. Triple-review (Codex self-adversarial + Gemini + fresh Opus) — same `simplify-with-review` skill cadence.
3. Local verification: §9.1 gates.
4. PR opened against `main`.
5. CI green → merge.

### 10.3 PR 2 implementation flow

1. Branch `harness/raw-methods-prevention-2-refactor` off PR 1's merged main.
2. Generate the authoritative refactor inventory with `cargo xtask check-test-surface --list`; every listed file must map to a batch/Spark packet before work starts.
3. For each batch (1..14 per §6.3, plus any generated straggler packets):
   a. Orchestrator (Claude) dispatches Sonnet with a batch file list, or Spark with exactly one file using the §6.3 work-packet template.
   b. Sonnet/Spark refactors files, runs per-file tests, commits per file or hands off a per-file patch for the orchestrator to commit.
   c. Orchestrator runs §6.4 verification gate.
   d. Codex adversarial review on the batch's commits.
   e. Fix any BLOCKs.
4. Activate the CI `cargo xtask check-test-surface` step only in the final PR 2 commit after the inventory is zero, so intermediate per-file commits remain bisectable.
5. Full PR 2 verification (§9.2 gates).
6. PR opened; merged.

### 10.4 PR 3 implementation flow + cluster 8ζ rebase

1. Branch `harness/raw-methods-prevention-3-lockdown` off PR 2's merged main.
2. Codex implements PR 3 against §1.3, §2.2, §2.3, §5.3, §5.4, §5.6.
3. Triple-review.
4. Verification (§9.3 gates).
5. PR opened; merged.

**Cluster 8ζ rebase** (after PR 3 merges):

The 8ζ branch carries:
- Function-name sweep commit (`c0850c6`) — fixes `generate_id_desc` → `heerid_next_desc` etc. **Already incorporated in PR 1 per §7.0.** During rebase, drop this commit if it is identical or resolve it as already-applied.
- CLAUDE.md additions (`d1b7fd1`, `9b198e3`, `497f6f8`, `b610e45`) — adds the "Tests must use djogi structs" guidance. **Conflicts with PR 1's CLAUDE.md rewrite (§3.7).** Resolution: drop the 8ζ CLAUDE.md commits during rebase; the harness's CLAUDE.md section supersedes them.
- `tests/integration/raw_methods_blacklist.rs` — runtime grep gate. **Now redundant; delete during rebase.**
- `PENDING_CLEANUP_133` allowlist constant in that file — also gone.
- 2 new raw-using tests (`phase8_t11_notify_roundtrip.rs`, `phase8_t8_7_outbox_tombstones.rs`) plus additional 8ζ pool/direct-driver tests under `phase8_zero_pool_*` and reset/notify coverage. **Refactor onto the typed surface or explicit pin/exception path as part of the rebase commit** — these are new files PR 2 didn't see.

**Conflict surface (be honest, BLOCK-2 fix):**
- `djogi/src/lib.rs` — PR 1 adds `pub mod __bypass;`. 8ζ adds `pub mod notify;` (T11). Both additive at module-list site: trivial three-way merge; accept both.
- `djogi/src/pg/pool.rs` — PR 3 demotes `with_client` to `pub(crate)` and 8ζ adds `url` / `pool_id` fields for notify. Resolution: accept both changes; keep `with_client` demoted, keep `url` / `pool_id` crate-private, and update any internal pool-level raw access through §5.3/§5.4. Do **not** rewrite notify to `raw_with_client`; notify needs the driver `Connection` half for `AsyncMessage` polling and must keep a dedicated direct connection.
- `djogi/src/notify.rs` — 8ζ uses `tokio_postgres::connect` directly. Resolution: add a narrow internal clippy allow or small internal helper documented in §5.3; the raw trait does not apply.
- `Cargo.toml` (workspace + `djogi/Cargo.toml`) — PR 1 adds `[lints]`, xtask member, `trait-variant`, `proc-macro2/span-locations`, and `notify`; 8ζ also adds `notify`. Resolution: accept one `notify = []` definition, keep explicit features, and avoid duplicate keys.
- `CLAUDE.md` — full conflict; PR 1 wins (drop 8ζ's section additions).

8ζ's rebase is therefore **non-trivial**. Treat it as a small follow-up branch, not a mechanical one-line rename: expect notify direct-driver handling, duplicate feature/dependency resolution, removal of the runtime blacklist, and refactors for every new raw/pool/direct-driver test added after PR 2. Estimate: 1–2 focused days, with its own adversarial review, not 1–2 hours.

---

## 11. Risks and open questions

### 11.1 Resolved decisions

- **Attribute name**: `deliberately_bypass_convention_with_raw_sql`. See §0.0.
- **CHECK constraint on outbox `action`**: not added; see §7.2.
- **Allowlist mechanism**: none. The runtime blacklist (8ζ-local) is dropped at rebase.
- **Pool/conn demotion**: yes; `pub(crate)` with bypass attribute the only public unlock.
- **`atomic` reshape**: no — preserves `IntoAtomicScope` polymorphism. Pin tests reach pool via `RawPoolAccessExt::raw_pool`.
- **Internal-callers pattern**: explicit `use crate::__bypass::RawAccessExt as DjogiRawAccessExt;` per `djogi/src` raw-call file, and `use djogi::__bypass::RawAccessExt as DjogiRawAccessExt;` in sibling workspace crates/examples; no JUSTIFICATION required outside `tests/`.
- **3-PR split**: yes — additive → refactor → lockdown.
- **JUSTIFICATION format**: `(djogi#<n>)` for non-pin raw escapes; `(PIN)` for tests/pin. Adopters file on djogi's tracker, not their own. Validation is syntactic attachment, not line proximity.
- **trait-method shape**: attempt `async fn` in trait + `#[trait_variant::make(_: Send)]` for the Send variant, with a required compile canary and a narrow fallback for stream methods only.
- **Macro on `mod foo;`**: explicit compile error.

### 11.2 Open

- **Q1 (counter-signal from Opus)**: would a value-marker shape (`RawSqlEscape<'_>` returned by an unlock fn that the bypass attribute brings into scope) be cleaner than the trait shape? Pro: more idiomatic Rust; can carry per-call metadata (issue number? span?). Con: requires every raw call to thread the marker; multi-call functions get verbose. Decision: **defer**. The trait shape is what v1 designed and what the proc macro injects via `use`. If we revisit later, it is a non-breaking refactor (the trait can be deprecated and replaced with the marker pattern in a future cluster).
- **Q2**: `trait_variant::make` is stable, but PR 1 must compile-prove it works with `async fn` returning a borrowed-self stream (`RawCursorStream<'a>`) via the fully-qualified trait canaries named in §1.1. If not, fall back to manual `impl Future` form for `raw_stream` and `raw_stream_with_fetch_size` only — these two methods would have hand-written returns, the other six would use `async fn`.
- **Q3**: rustc 1.75 MSRV is no longer the active floor; current workspace `rust-version` is 1.95. No MSRV raise is needed for async-fn-in-trait, but adding `trait-variant` must still respect 1.95-compatible dependency resolution.
- **Q4**: xtask member in workspace — does adding a workspace member affect downstream `path = "../djogi"` consumers (e.g. sister crates)? It should not (xtask is a binary, not a library), but verify in CI.
- **Q5**: adopter API break — today an adopter's production code can write `ctx.raw_execute(...)`. After PR 3, they must add `#[djogi::deliberately_bypass_convention_with_raw_sql]` on the calling fn (or consciously `use djogi::__bypass::RawAccessExt;` in their own non-test code). This repo's `tests/` may not reference `djogi::__bypass` directly; xtask enforces that local policy. Per `project_djogi_prepublish.md`, djogi is pre-publish; the break is acceptable. Document in release notes.
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
5. `chore(deps): add trait-variant; enable proc-macro2 span-locations; define notify feature`.
6. `feat(djogi): introduce __bypass::RawAccessExt + RawPoolAccessExt as additive sealed traits`. Module rustdoc copied from the docs draft. Existing `pub raw_*` inherent methods remain, so PR 1 canaries must use fully-qualified trait calls to prove the trait path instead of method-call syntax. Include compile canaries for stream trait calls.
7. `feat(xtask): add cargo xtask check-justifications validator and check-test-surface lockdown scan` (command exists but CI activation for `check-test-surface` waits until PR 2's final commit).
8. `chore(pk): incorporate 8ζ c0850c6 HeeRanjID function-name sweep`.
9. `feat(migrate): synthesise {table}_outbox in projection (#134)`.

**Phase C — Pin tests (the carve-out)**
10. `test(pin): add pin tests for 8 raw APIs + raw_pool_access (9 files)`. All 9 carry the bypass attribute and `JUSTIFICATION (PIN)` comments.

**Phase D — CI**
11. `ci: explicit feature lists; add check-justifications + pin suite steps to ci.yml; check-test-surface deferred to final PR 2 commit; trybuild deferred to PR 3`.

### 12.2 PR 2 — test refactor

12. `test(integration): refactor phase1 tests off raw_*` (Sonnet batch 1 / Spark packet).
13. `test(integration): refactor phase2 tests off raw_*` (Sonnet batch 2 / Spark packets).
14. `test(integration): refactor phase3 tests off raw_*`.
15. `test(integration): refactor phase4 tests off raw_*`.
16. `test(integration): refactor phase5 tests off raw_*`.
17. `test(integration,pin): relocate phase5_zero_raw_in_atomic to tests/pin/`.
18. `test(integration): refactor phase6 tests off raw_*`.
19. `test(integration): refactor phase7 tests off raw_*`.
20. `test(integration): refactor phase7_5 tests off raw_*`.
21. `test(integration): refactor phase7_zero2 tests off raw_*`.
22. `test(integration): refactor phase8 compose+hooks+role tests off raw_*`.
23. `test(integration): refactor phase8 t7+t8 (Punnu) tests off raw_*`.
24. `test(integration): refactor phase8_zero tests off raw_*`.
25. `test(integration): refactor phase8 CLI/pool stragglers and djogi-cli integration tests off raw_*`.
26. `ci: activate cargo xtask check-test-surface after ordinary workspace integration tests are zero-raw`.

(Codex adversarial-reviews each commit between batches; commits split further if any review surfaces a BLOCK that needs a separate fixup.)

### 12.3 PR 3 — lockdown

27. `chore(lints): activate workspace clippy.toml disallowed_methods with per-crate/internal allow sites`.
28. `feat(djogi): demote raw_* to RawAccessExt and add alias imports to all workspace raw callers` (the API break — atomic commit; covers `djogi/src`, `djogi-cli/src`, and `examples/elephant-tracker`).
29. `feat(djogi): demote pool() and conn() to pub(crate); RawPoolAccessExt::raw_pool / raw_conn unlock`.
30. `feat(djogi): demote DjogiPool::with_client to pub(crate); RawPoolAccessExt::raw_with_client unlock`.
31. `test(compile-fail): add trybuild fixtures asserting raw_* / pool / with_client don't resolve on bare DjogiContext`.
32. `ci: activate trybuild compile-fail gate in ci.yml`.

### 12.4 Discipline

Each commit is atomic, passes its own tests, and is bisectable. Every implementation commit (4–32) cites the spec section it enacts in its message body. The reviewer cycle (Codex + Gemini + Opus) checks contract adherence at every round — implementation that diverges from the docs without updating the docs first is a hard BLOCK.

---

## 13. What this plan does NOT cover (explicit non-goals)

- Adopter-side enforcement in their own repos (only a CLAUDE.md hint).
- A `djogi lint` CLI subcommand (the xtask is the validator).
- A separate `djogi-test-harness` crate for adopters.
- Adopter-side prevention of manual `use djogi::__bypass::RawAccessExt`; the module is public-but-hidden for conscious opt-out. This repository's `tests/` ban direct `djogi::__bypass` references via xtask, but external repos must enforce their own policy if they want the same rule.
- The notify watcher-died lifecycle gap (GH #131 — separate cluster).
- The `target/djogi_outbox/<table>_outbox.sql` build-time emission — runtime/projection-side synthesis is sufficient for this PR's scope.
- Refactoring `djogi/src/migrate/projection.rs` itself onto a different shape — this plan adds the outbox synthesis but doesn't restructure the existing projection code.

---

## End of plan

Reviewers: surface any gap that would let an ordinary integration test still reach `raw_*`, `pool()`, `conn()`, `with_client`, `batch_execute`, `djogi::__bypass`, or `tokio_postgres::*` direct without the intended harness path. Surface any failure mode that would let the cluster-8ζ rebase break. Surface any ergonomic or maintainability concern that argues for a different mechanism. Specifically check whether the v1 BLOCK fixes are adequate and whether the v3 BLOCK fixes remain internally consistent. Output: ALLOW / BLOCK with concrete findings.
