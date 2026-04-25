> [Back to README](../../../ReadMe.MD) | [All Specs](../index.md) | [Maahi](./index.md)

# Maahi — Security

Maahi's security floor is layered defense plus an absolute rule: all field-level access decisions are re-verified server-side on every mutation. The form is a hint; the server is the truth.

## CSRF — Triple Stack

1. **`SameSite=Strict` session cookie** with `HttpOnly` and `Secure` flags. Defense in depth; not bulletproof on its own (subdomain takeovers, browser bugs).
2. **Custom request header** `X-Maahi-CSRF` carrying the session's CSRF token. Cross-origin requests cannot set this header without CORS preflight, which Maahi never permits on mutation endpoints. Dioxus full-stack server functions use a custom header naturally — this layer is on by construction.
3. **Origin/Referer check** on every mutation server function. The originating header must match the deployment's configured origin (`[admin].origin` in `Djogi.toml`, or derived from the running URL).

A request missing any of the three is rejected before reaching the descriptor-aware mutation path.

## Sessions — Rotation Discipline

- Session token is generated cryptographically per login, stored as `argon2` hash in `_admin_sessions.token_hash`. The cookie carries the plaintext; the database carries only the hash.
- Session ID rotates on login, password change, role change, and tenant switch. Privilege change without rotation is a fixation hazard.
- Idle timeout: configurable in `[admin].session_idle` (default `30m`). Updated on each authenticated request via `last_seen_at`.
- Absolute timeout: configurable in `[admin].session_max` (default `12h`). Independent of activity.
- Logout revokes the session row.
- Login rate limiting: a small in-memory leaky bucket per `(email, ip_inet)` rejects brute-force attempts with a `429`. Limits configurable in `[admin].login_rate_limit`.

## Server-Side Write Enforcement

Every Update and Create server function performs the following pipeline before any database write:

1. Resolve the requesting user's effective `(scope, actions)` for the target model — including parent-chain resolution and per-model overrides.
2. Compute the **writable field set**: fields exposed to scope, not marked `admin_readonly`, not `expose(none)`.
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

> [Back to README](../../../ReadMe.MD) | [All Specs](../index.md) | [Maahi](./index.md)
