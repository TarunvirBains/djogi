> [Back to README](../../../README.md) | [All Specs](../index.md) | [Maahi](./index.md)

# Maahi — Configuration and CLI

## `Djogi.toml` `[admin]` Block

Maahi's configuration lives in the `[admin]` block of `Djogi.toml`:

```toml
[admin]
enabled               = true
path                  = "/_admin/"            # URL mount point
origin                = "https://app.example" # used in Origin/Referer check
session_idle          = "30m"                 # idle timeout
session_max           = "12h"                 # absolute timeout
pending_action_ttl    = "24h"                 # approval-queue expiry
inline_page_size      = 10                    # M2M inline default
inline_bulk_threshold = 25                    # inline removals at-or-above this count route a parent save through the dual-control approval flow as an `InlineSave` pending action (sibling to `BulkDelete`)
default_page_size     = 25                    # list view default
fk_preload_threshold  = 200                   # FK target rowcount at-or-above which the dropdown switches from preload to typeahead — see UI Surface
                                              # Auto-detected at startup from pg_class.reltuples; per-FK override via #[field(admin_fk_widget = "preload" | "typeahead")]
login_rate_limit_per_ip    = "20/5m"          # per-IP limiter (caps total auth volume from one source)
login_rate_limit_per_email = "10/5m"          # per-email limiter (caps total auth volume against one account)
                                              # Both must accept; either failing returns 429.
                                              # Multi-instance deployments require shared state — see security.md
csrf_secret_env       = "DJOGI_ADMIN_CSRF_SECRET"     # env var holding signing key
session_secret_env    = "DJOGI_ADMIN_SESSION_SECRET"  # env var holding signing key
# multi_tenant       = false                          # auto-detected from registered RLS-enabled models;
                                                      # override only if auto-detection is wrong for the deployment
```

Secrets — CSRF signing key, session cookie signing key — live in environment variables only, never in `Djogi.toml`, consistent with the `DATABASE_URL` rule in [Configuration](../configuration.md).

Missing env vars at startup cause Maahi to refuse to mount, with a clear diagnostic naming the missing variable. There is no fallback derivation, no in-process default — these are infrastructure config and must be provisioned.

## Operator Surface

Maahi's admin CLI commands are planned, but they are not registered in the shipped `djogi` binary yet. Until that surface lands, Maahi bootstrap and password-reset flows are specified as operator requirements rather than executable `djogi admin` commands:

```bash
# Bootstrap the first administrator (one-time, requires audit DB access)
djogi admin set-password --superuser <email>   # planned; not shipped today

# Reset a user's password (operator-side fallback; users have no email-driven flow in v1)
djogi admin reset-password <email>             # planned; not shipped today

# Build the Maahi WASM bundle for production deployment
djogi admin build [--release]                  # planned; not shipped today

# Print the admin URL and current login status (development convenience)
djogi admin info                               # planned; not shipped today
```

Forgot-password via email is not part of v1 — the framework has no notification infrastructure to call into. Operators reset passwords via the CLI fallback. An `EmailSender` trait plus integrated forgot-password is anchored to a future Notification Infrastructure phase ([Phase Map](./phase-map.md)).

## Bootstrap Flow

1. Provision the audit database (already required by [Logging](../logging.md))
2. Set the env vars: `DJOGI_ADMIN_CSRF_SECRET`, `DJOGI_ADMIN_SESSION_SECRET`, `DATABASE_URL`, `CRUD_LOG_URL`
3. Compose Maahi's migrations with `djogi migrations compose` — the descriptor differ emits up/down SQL pairs for Maahi's tables (`_admin_users`, `_admin_roles`, `_admin_role_visage_perms`, `_admin_role_model_perms`, `_admin_sessions`, `_admin_pending_actions`). Per the per-target migration model documented in [Logging](../logging.md), Maahi's tables live in the `crud_log` (audit) database; compose main-DB migrations separately if this is a greenfield deployment. Apply the composed plans with `djogi migrations apply` plus node identity (`--node-id`, `HEER_NODE_ID`, or `--single-node-dev`) or via the library API `djogi::migrate::apply_plan`
4. Create the first admin user through the implementation-specific bootstrap path until the planned `djogi admin set-password --superuser <email>` command ships. The bootstrap operation creates the first user, prompts or receives the password through operator-controlled secret input, hashes via argon2, and writes to `_admin_users` with `is_superuser = TRUE`
5. Start the application; navigate to `/_admin/`; log in with the bootstrap credentials
6. From inside Maahi, the superuser creates additional roles and users
7. **Before relying on `BulkDelete` or above-threshold `InlineSave`**, provision a second admin. The second admin must hold *every action permission required by the operation they are expected to approve*, not merely `BulkDelete` — the approver-coverage rule documented in [Operations](./operations.md) requires the approver to satisfy the full action set the package executes. For `BulkDelete` from a changelist, that is `BulkDelete` on the target model. For `InlineSave`, that may include `Update` on the parent model plus `Create` / `Update` / `Delete` / `BulkDelete` on the through model — the package's actual contents determine the requirement. The dual-control approval gate requires `approver ≠ requester` and does not relax for single-admin deployments; the bootstrap state alone cannot execute either of v1's two approval-gated action kinds. Practical guidance: a second superuser is the simplest provisioning path because superusers cover any action set; a role-bounded second admin must be sized to the operations it will approve.

The bootstrap flow is one-time. Subsequent superuser additions go through Maahi itself once an existing superuser can promote another user; the planned `djogi admin set-password --superuser` command will provide a re-runnable CLI fallback when the admin command surface ships.

---

> [Back to README](../../../README.md) | [All Specs](../index.md) | [Maahi](./index.md)
