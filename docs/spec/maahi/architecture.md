> [Back to README](../../../ReadMe.MD) | [All Specs](../index.md) | [Maahi](./index.md)

# Maahi — Planned Architecture and Auth Substrate

## Dioxus Full-Stack on Axum

Maahi is planned as a single Dioxus full-stack application:

- **Server functions** (Dioxus `#[server]`) handle every state-changing operation. Each call carries the session cookie, a CSRF header, and is dispatched against `DjogiContext` so transactions, RLS tenant context, and the descriptor are all native at the call site.
- **Hydrated client** will ship as a WASM bundle; `dx bundle` is the planned build pipeline, integrated into a future `djogi admin build` for predictable production output. Asset serving is handled by Dioxus's own Axum integration — no separate `tower_http::services::ServeDir` wiring.
- **Component tree** is descriptor-driven: a `ModelView` component takes a `ModelDescriptor` plus the requesting role's effective `(visibility, actions)` — visage grants resolved across the role chain plus per-model action overrides, per [RBAC](./rbac.md) — and renders list, detail, edit, and create surfaces. Per-field widgets dispatch from `FieldDescriptor::ty` using the table in [UI Surface](./ui.md).

Because Dioxus components are pure Rust, the same component tree is reachable from `dioxus-desktop` for adopters who want a native admin shell. Desktop packaging is not a Phase 10 deliverable, but the renderer choice keeps that path open.

## Web Framework Integration

When Maahi ships, it is expected to require the `axum` feature flag in addition to `admin`. Dioxus full-stack runs as an Axum router; merging it into an adopter's existing Axum router is the planned integration path.

```toml
# Planned Cargo.toml shape; not available in v0.1.0-alpha.
djogi = { version = "0.1", features = ["admin", "axum"] }
```

Adopters on other web frameworks would run Maahi as a separate process and reverse-proxy `/_admin/` to it. The previous spec's per-framework integration story is narrowed: Maahi is Axum-only by design.

## Workspace Layout

The target Maahi layout is a separate crate within Djogi's monorepo workspace. This crate is not present in the v0.1.0-alpha workspace yet:

```text
djogi/
  djogi/                ← framework library
  djogi-macros/         ← proc macros
  djogi-cli/            ← djogi binary
  djogi-shell/          ← planned Rhai REPL
  djogi-maahi/          ← planned admin console (this spec)
```

The planned `djogi` `admin` feature will pull in `djogi-maahi` as an optional dep, and `djogi::maahi::*` will re-export the public API. From the adopter's perspective, the intended future command is `cargo add djogi --features admin`; in v0.1.0-alpha, that admin surface is not shipped.

**Why a separate crate** (when other specialized features stay as feature flags within `djogi`): Dioxus full-stack is a whole UI framework with WASM-target builds and pre-1.0 release cadence. Pulling Dioxus into `djogi` directly would inflate every adopter's lock file with UI-framework deps even when admin is opt-in — Cargo features gate compilation but not lock-file membership — and Dioxus version churn would force `djogi` minor bumps. Carving Maahi out lets `djogi-maahi` track Dioxus releases independently while `djogi` core stays stable.

The carve-out is Maahi-specific. Spatial, vector, outbox publisher backends, and other specialized features remain feature flags within `djogi` per the standard rule.

## Auth Substrate — Hybrid `_admin_users`

The planned Maahi surface authenticates against its own user table, isolated from the application's auth surface but built on the same primitives. This is the hybrid pattern: separate data, shared cryptography.

```sql
-- Lives in the audit DB (crud_log_url), not the app DB.
-- Survives `djogi db reset` on the app database.

CREATE TABLE _admin_users (
    id            BIGINT PRIMARY KEY DEFAULT heerid_next_desc(),
    email         TEXT UNIQUE NOT NULL,
    password_hash TEXT NOT NULL,           -- argon2 via djogi::auth::PasswordHash (Phase 5.5)
    role_id       BIGINT REFERENCES _admin_roles(id) ON DELETE RESTRICT,
                                           -- explicit RESTRICT: role deletion is blocked while users are
                                           -- assigned; the role-edit page enforces reassign-first (see rbac.md
                                           -- "Role Deletion UX")
    is_superuser  BOOLEAN NOT NULL DEFAULT FALSE,
    tenant_scope  TEXT,                    -- nullable; UI-level requirement is deployment-mode-dependent (see Multi-Tenancy section below)
    expires_at    TIMESTAMPTZ,             -- NULL = no expiry; for time-bounded contractor access
    created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_login_at TIMESTAMPTZ
);

CREATE TABLE _admin_sessions (
    id                   BIGINT PRIMARY KEY DEFAULT heerid_next_desc(),
    user_id              BIGINT NOT NULL REFERENCES _admin_users(id) ON DELETE CASCADE,
    token_hash           TEXT NOT NULL,    -- HMAC-SHA256 keyed by session_secret_env over the session token;
                                           -- the cookie carries the plaintext token, the DB carries only the
                                           -- HMAC. Deterministic + indexed so per-request session lookup is a
                                           -- single equality probe. Argon2 stays on _admin_users.password_hash
                                           -- where slow-by-design is the right primitive; HMAC-SHA256 is the
                                           -- right primitive for short-lived secret-token storage.
    csrf_token           TEXT NOT NULL,    -- per-session, sent in X-Maahi-CSRF on every mutation
    current_tenant_scope TEXT,             -- per-session active tenant for cross-tenant users (see Multi-Tenancy
                                           -- below). For non-cross-tenant users, derived from
                                           -- _admin_users.tenant_scope at login. NULL in single-tenant mode.
    issued_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at           TIMESTAMPTZ NOT NULL,
    last_seen_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    ip_inet              INET,
    user_agent           TEXT
);

CREATE UNIQUE INDEX _admin_sessions_token_hash_idx ON _admin_sessions (token_hash);
CREATE INDEX        _admin_sessions_user_id_idx     ON _admin_sessions (user_id);
CREATE INDEX        _admin_sessions_expires_at_idx  ON _admin_sessions (expires_at);
```

**Why the audit DB.** A `djogi db reset` on the application database during development must not lock administrators out. The audit DB is already provisioned by the three-database architecture defined in [Logging](../logging.md), so Maahi tables ride that pool without adding infrastructure.

**Why hybrid (not direct DjogiAuth reuse).** If the application's own auth substrate is mid-migration or wedged, administrators must still be able to log in to investigate. Sharing primitives (`PasswordHash`, session-cookie crypto, argon2 parameters from Phase 5.5) is desirable; sharing the user table is not.

**Bootstrap.** `planned `djogi admin` set-password --superuser <email>` creates the first user with `is_superuser = TRUE`. Subsequent users are created from inside Maahi by anyone with `is_superuser = TRUE` or the `manage_users` system permission ([Operations](./operations.md)).

## Multi-Tenancy

Maahi adapts to the application's tenancy model. Phase 5's RLS substrate and `set_tenant` machinery are non-negotiable inputs to admin queries when the application is multi-tenant; running Maahi against a multi-tenant deployment without honoring them silently leaks across tenants. Single-tenant deployments don't pay any tenant-related cost.

Maahi detects multi-tenant mode at startup by inspecting whether any registered model declares tenant-scoped RLS (per Phase 5's `tenant_key` annotation). Operators can override the auto-detection via `[admin].multi_tenant = true|false` in `Djogi.toml` — useful when a deployment is staging a tenancy transition. The mode is fixed at process startup; flipping it requires a restart.

Two relevant columns on the Maahi tables; their semantics are deployment-mode-dependent:

- `_admin_users.tenant_scope: TEXT` — nullable column. In multi-tenant mode, the role-management UI requires a non-NULL value unless the user is a superuser or has been assigned a role with `cross_tenant = TRUE`. In single-tenant mode, the field is hidden in the UI and left NULL on every user.
- `_admin_roles.cross_tenant: BOOLEAN` — non-superuser roles allowed cross-tenant access. Useful in multi-tenant deployments for "platform support agent" roles that span customers; ignored in single-tenant mode.

In **multi-tenant** deployments, the login flow captures the active tenant into `_admin_sessions.current_tenant_scope` at session-issuance time. Two flows depending on the user shape:

- **Single-tenant user** (`_admin_users.tenant_scope` non-NULL): login derives `current_tenant_scope` directly from the user row at issuance — no picker prompt.
- **Cross-tenant user** (`_admin_users.tenant_scope IS NULL`, role `cross_tenant = TRUE`): login does *not* issue a session until the user picks a tenant. The flow is:
  1. Credential check (email + password verified against `_admin_users.password_hash`).
  2. Maahi issues a **short-lived signed one-time login ticket** — an HMAC-SHA256 token keyed by `session_secret_env`, body `{user_id, issued_at}`, TTL 60 seconds — and returns it to the client; no `_admin_sessions` row exists yet, no session cookie is set.
  3. Client renders the tenant picker. The tenant-selection POST carries the ticket (in a request header, not a cookie — no auth state is being persisted yet).
  4. Maahi verifies the ticket signature, checks the TTL, confirms the ticket has not been consumed (one-time use enforced via a small bounded in-memory set keyed by ticket id with TTL-aligned eviction; multi-instance deployments back this with the audit DB), and validates the picked tenant is reachable for `user_id`'s assigned role.
  5. On success, Maahi consumes the ticket, writes the `_admin_sessions` row with `current_tenant_scope = picked_tenant`, and issues the session cookie. There is no "no tenant set" or "all-tenants" intermediate state — sessions always have a non-NULL `current_tenant_scope` in multi-tenant mode.
  6. On ticket failure (invalid signature, expired, already consumed, tenant unreachable), the request is rejected with no session and no information leak; the user must restart from credential entry.

  This binding mechanism prevents an unauthenticated tenant-selection POST from creating a session, and prevents a stolen ticket from being replayed past its 60-second window or after first use.

Every server function dispatches against a `DjogiContext` whose `set_tenant(session.current_tenant_scope)` has already been called by middleware. RLS does the rest. Switching the picker rotates the session (per the rotation discipline in [Security](./security.md)) — the rotated row is written with the new `current_tenant_scope` value atomically. Cookie tampering cannot move a session across tenants because the active tenant lives server-side on the session row, not in the cookie.

In **single-tenant** deployments, `set_tenant` is never called on the admin's `DjogiContext`, `current_tenant_scope` is left NULL on every session row, the picker is hidden, `cross_tenant` is irrelevant, and the `tenant_scope` UI-level requirement does not apply.

---

> [Back to README](../../../ReadMe.MD) | [All Specs](../index.md) | [Maahi](./index.md)
