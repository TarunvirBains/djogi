> [Back to README](../../../ReadMe.MD) | [All Specs](../index.md) | [Maahi](./index.md)

# Maahi — Architecture and Auth Substrate

## Dioxus Full-Stack on Axum

Maahi is a single Dioxus full-stack application:

- **Server functions** (Dioxus `#[server]`) handle every state-changing operation. Each call carries the session cookie, a CSRF header, and is dispatched against `DjogiContext` so transactions, RLS tenant context, and the descriptor are all native at the call site.
- **Hydrated client** ships as a WASM bundle; `dx bundle` is the build pipeline, integrated into `cargo djogi admin build` for predictable production output. Asset serving is handled by Dioxus's own Axum integration — no separate `tower_http::services::ServeDir` wiring.
- **Component tree** is descriptor-driven: a `ModelView` component takes a `ModelDescriptor` plus the requesting role's effective `(scope, actions)` and renders list, detail, edit, and create surfaces. Per-field widgets dispatch from `FieldDescriptor::ty` using the table in [UI Surface](./ui.md).

Because Dioxus components are pure Rust, the same component tree is reachable from `dioxus-desktop` for adopters who want a native admin shell. Desktop packaging is not a Phase 10 deliverable, but the renderer choice keeps that path open.

## Web Framework Integration

Maahi requires the `axum` feature flag in addition to `admin`. Dioxus full-stack runs as an Axum router; merging it into an adopter's existing Axum router is the supported integration path.

```toml
# Cargo.toml
djogi = { version = "0.1", features = ["admin", "axum"] }
```

Adopters on other web frameworks run Maahi as a separate process and reverse-proxy `/_admin/` to it. The previous spec's per-framework integration story is narrowed: Maahi is Axum-only by design.

## Workspace Layout

Maahi is its own crate within Djogi's monorepo workspace:

```text
djogi/
  djogi/                ← framework library
  djogi-macros/         ← proc macros
  djogi-cli/            ← cargo djogi binary
  djogi-shell/          ← Rhai REPL
  djogi-maahi/          ← admin console (this spec)
```

`djogi`'s `admin` feature pulls in `djogi-maahi` as an optional dep, and `djogi::maahi::*` re-exports the public API. From the adopter's perspective, `cargo add djogi --features admin` is the only thing that changes; the dep tree just gains the Maahi crate transparently.

**Why a separate crate** (when other specialized features stay as feature flags within `djogi`): Dioxus full-stack is a whole UI framework with WASM-target builds and pre-1.0 release cadence. Pulling Dioxus into `djogi` directly would inflate every adopter's lock file with UI-framework deps even when admin is opt-in — Cargo features gate compilation but not lock-file membership — and Dioxus version churn would force `djogi` minor bumps. Carving Maahi out lets `djogi-maahi` track Dioxus releases independently while `djogi` core stays stable.

The carve-out is Maahi-specific. Spatial, vector, outbox publisher backends, and other specialized features remain feature flags within `djogi` per the standard rule.

## Auth Substrate — Hybrid `_admin_users`

Maahi authenticates against its own user table, isolated from the application's auth surface but built on the same primitives. This is the hybrid pattern: separate data, shared cryptography.

```sql
-- Lives in the audit DB (crud_log_url), not the app DB.
-- Survives `cargo djogi db reset` on the app database.

CREATE TABLE _admin_users (
    id            BIGINT PRIMARY KEY DEFAULT generate_id(),
    email         TEXT UNIQUE NOT NULL,
    password_hash TEXT NOT NULL,           -- argon2 via djogi::auth::PasswordHash (Phase 5.5)
    role_id       BIGINT REFERENCES _admin_roles(id),
    is_superuser  BOOLEAN NOT NULL DEFAULT FALSE,
    tenant_scope  TEXT,                    -- nullable; UI-level requirement is deployment-mode-dependent (see Multi-Tenancy section below)
    expires_at    TIMESTAMPTZ,             -- NULL = no expiry; for time-bounded contractor access
    created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_login_at TIMESTAMPTZ
);

CREATE TABLE _admin_sessions (
    id            BIGINT PRIMARY KEY DEFAULT generate_id(),
    user_id       BIGINT NOT NULL REFERENCES _admin_users(id) ON DELETE CASCADE,
    token_hash    TEXT NOT NULL,           -- argon2 of the session token; cookie carries the plaintext
    csrf_token    TEXT NOT NULL,           -- per-session, sent in X-Maahi-CSRF on every mutation
    issued_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at    TIMESTAMPTZ NOT NULL,
    last_seen_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    ip_inet       INET,
    user_agent    TEXT
);

CREATE INDEX _admin_sessions_user_id_idx       ON _admin_sessions (user_id);
CREATE INDEX _admin_sessions_expires_at_idx    ON _admin_sessions (expires_at);
```

**Why the audit DB.** A `cargo djogi db reset` on the application database during development must not lock administrators out. The audit DB is already provisioned by the three-database architecture defined in [Logging](../logging.md), so Maahi tables ride that pool without adding infrastructure.

**Why hybrid (not direct DjogiAuth reuse).** If the application's own auth substrate is mid-migration or wedged, administrators must still be able to log in to investigate. Sharing primitives (`PasswordHash`, session-cookie crypto, argon2 parameters from Phase 5.5) is desirable; sharing the user table is not.

**Bootstrap.** `cargo djogi admin set-password --superuser <email>` creates the first user with `is_superuser = TRUE`. Subsequent users are created from inside Maahi by anyone with `is_superuser = TRUE` or the `manage_users` system permission ([Operations](./operations.md)).

## Multi-Tenancy

Maahi adapts to the application's tenancy model. Phase 5's RLS substrate and `set_tenant` machinery are non-negotiable inputs to admin queries when the application is multi-tenant; running Maahi against a multi-tenant deployment without honoring them silently leaks across tenants. Single-tenant deployments don't pay any tenant-related cost.

Maahi detects multi-tenant mode at startup by inspecting whether any registered model declares tenant-scoped RLS (per Phase 5's `tenant_key` annotation). Operators can override the auto-detection via `[admin].multi_tenant = true|false` in `Djogi.toml` — useful when a deployment is staging a tenancy transition. The mode is fixed at process startup; flipping it requires a restart.

Two relevant columns on the Maahi tables; their semantics are deployment-mode-dependent:

- `_admin_users.tenant_scope: TEXT` — nullable column. In multi-tenant mode, the role-management UI requires a non-NULL value unless the user is a superuser or has been assigned a role with `cross_tenant = TRUE`. In single-tenant mode, the field is hidden in the UI and left NULL on every user.
- `_admin_roles.cross_tenant: BOOLEAN` — non-superuser roles allowed cross-tenant access. Useful in multi-tenant deployments for "platform support agent" roles that span customers; ignored in single-tenant mode.

In **multi-tenant** deployments, the login flow captures `tenant_scope` into the session. Every server function dispatches against a `DjogiContext` whose `set_tenant(tenant_scope)` has already been called by middleware. RLS does the rest. Cross-tenant users get a tenant picker in the navigation bar; switching the picker re-issues `set_tenant` for subsequent queries.

In **single-tenant** deployments, `set_tenant` is never called on the admin's `DjogiContext`, the picker is hidden, `cross_tenant` is irrelevant, and the `tenant_scope` UI-level requirement does not apply.

---

> [Back to README](../../../ReadMe.MD) | [All Specs](../index.md) | [Maahi](./index.md)
