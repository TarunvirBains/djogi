> [Back to README](../../../README.md) | [All Specs](../index.md) | [Maahi](./index.md)

# Maahi — Security

Maahi's security floor is layered defense plus an absolute rule: all field-level access decisions are re-verified server-side on every mutation. The form is a hint; the server is the truth.

## CSRF — Triple Stack

1. **`SameSite=Strict` session cookie** with `HttpOnly` and `Secure` flags. Defense in depth; not bulletproof on its own (subdomain takeovers, browser bugs).
2. **Custom request header** `X-Maahi-CSRF` carrying the session's CSRF token. Cross-origin requests cannot set this header without CORS preflight, which Maahi never permits on mutation endpoints. Dioxus full-stack server functions use a custom header naturally — this layer is on by construction.
3. **Origin/Referer check** on every mutation server function. The originating header must match the deployment's configured origin (`[admin].origin` in `Djogi.toml`, or derived from the running URL).

A request missing any of the three is rejected before reaching the descriptor-aware mutation path.

## Sessions — Rotation Discipline

- Session token is generated cryptographically per login, stored as **HMAC-SHA256** keyed by `session_secret_env` over the plaintext in `_admin_sessions.token_hash`. The cookie carries the plaintext; the database carries only the HMAC. Deterministic + indexed (the `_admin_sessions_token_hash_idx` UNIQUE INDEX from [Architecture](./architecture.md)) so per-request session lookup is a single equality probe in microseconds. Argon2 is reserved for `_admin_users.password_hash` where slow-by-design is the right primitive against offline brute force; HMAC-SHA256 is the right primitive for short-lived secret-token storage where lookup speed is on the hot path.
- Session ID rotates on login, password change, role change, and tenant switch. The rotated row carries the new tenant value in `_admin_sessions.current_tenant_scope` atomically when the trigger is a tenant switch. Privilege change without rotation is a fixation hazard.
- Idle timeout: configurable in `[admin].session_idle` (default `30m`). Updated on each authenticated request via `last_seen_at`.
- Absolute timeout: configurable in `[admin].session_max` (default `12h`). Independent of activity.
- Logout revokes the session row.

## Login Rate Limiting

Two parallel leaky-bucket limiters gate login; both must accept for the request to proceed:

- **Per-IP** limiter, keyed by `ip_inet`. Caps total auth volume from a source. Defeats single-IP brute force.
- **Per-email** limiter, keyed by `email`. Caps total auth volume against an account. Defeats credential-stuffing across many IPs (residential proxies, botnets) targeting one account.

Both must accept; failure of either returns `429`. The compound `(email, ip)` key alone is the weakest of the three standard shapes — a credential-stuffing attacker rotating IPs gets a fresh bucket per source and trivially bypasses it. Two parallel limiters is the OWASP ASVS / NIST 800-63B baseline.

Limits configurable per limiter via two separate `[admin]` keys: `login_rate_limit_per_ip` (per-IP rate) and `login_rate_limit_per_email` (per-email rate). See [Configuration](./configuration.md) for defaults and rate-string format.

State storage: single-instance deployments may use process-local in-memory state. Multi-instance deployments must use shared state — the audit DB Maahi already provisions (the `crud_log` target) is the natural home; an `_admin_login_rate_limit` table or equivalent backing store is acceptable. The spec does not mandate the storage shape, only that multi-replica deployments cannot rely on per-process buckets (which would scale the limiter the wrong direction — N replicas = N× the configured rate).

## Server-Side Write Enforcement

Every Update and Create server function performs the following pipeline before any database write:

1. Resolve the requesting user's effective `(visibility, actions)` for the target model — visage grants resolved across the role chain, optionally extended by `view_full_struct` / `write_full_struct`, intersected with per-model action overrides.
2. Compute the **writable field set**: fields in the role's effective editable set (granted-edit visages plus `write_full_struct` if held), minus `admin_readonly`, minus `expose(none)`.
3. Reject the request if the submitted payload contains any field outside the writable set. Silent filtering hides tampering attempts; explicit rejection surfaces them. The error is logged to the event log with the offending field name.
4. Run `AdminClean::clean()` if the model implements it (see [UI Surface](./ui.md)).
5. Apply the change through the descriptor-aware ORM path so dirty tracking, audit log mirroring, and `Tracked` semantics are honored.

The same enforcement is repeated on every Delete (via row-level `can_actually_delete` plus tenant check) and every Bulk action (with the additional approval gate from [Operations](./operations.md)).

## Secrets Hygiene

CSRF signing key and session-cookie signing key live in environment variables, not in `Djogi.toml`. The TOML carries only the env-var *names*:

```toml
[admin]
csrf_secret_env    = "DJOGI_ADMIN_CSRF_SECRET"
session_secret_env = "DJOGI_ADMIN_SESSION_SECRET"
```

Missing env vars at startup cause Maahi to refuse to mount, with a clear diagnostic naming the missing variable. There is no fallback derivation, no in-process default — these are infrastructure config and must be provisioned.

---

> [Back to README](../../../README.md) | [All Specs](../index.md) | [Maahi](./index.md)
