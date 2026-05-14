> [Back to README](../../ReadMe.MD) | [All Guides](./index.md)

# Secrets Hygiene

Djogi ships a lightweight secret-pattern scanner so credential-shaped
text does not silently land in commits, pull requests, public issues,
or copy-pasted diagnostic snippets. The scanner is a **guardrail**, not
a full DLP product — it catches obvious patterns with high precision
and a documented allowlist for intentional fixtures.

Run it via `cargo xtask check-secrets`. The workflow CI lane runs the
same command on every PR.

## What the scanner detects

| Pattern | Example (placeholders, not real values) |
|---|---|
| URLs with embedded `user:password` | `postgres://alice:hunter2@db.prod.example/myapp` |
| Known secret env-var assignments | `PGPASSWORD=...`, `POSTGRES_PASSWORD=...`, `DATABASE_URL=...`, `STRIPE_SECRET_KEY=...` |
| Credential-suffix env-var assignments | `MY_SERVICE_TOKEN=...`, `*_PASSWORD=...`, `*_SECRET=...`, `*_API_KEY=...`, `*_CLIENT_SECRET=...` |
| PEM private-key block headers | `-----BEGIN PRIVATE KEY-----`, `-----BEGIN RSA PRIVATE KEY-----`, etc. |

The scanner never echoes the raw matched value — every finding is
reported as a structural template (`postgres://user:<REDACTED>@host`,
`PGPASSWORD=<REDACTED N bytes>`) so log output is safe to share for
triage.

## What the scanner does *not* detect

- Generic high-entropy strings without a credential-shaped context.
  Add the entropy heuristic only with a curated allowlist of
  intentional base64 / hex fixture sites — out of scope for now.
- Historical commits, remote branches, or live GitHub issue bodies.
  Those require a separate sweep with appropriately scoped tooling.
- The `.pgpass` `host:port:db:user:password` shape. The recommendation
  is to keep `.pgpass` out of the repository entirely; the file pattern
  is too easy to confuse with unrelated colon-separated text.

## Modes

### Repo sweep (default)

```bash
cargo xtask check-secrets
```

Walks every tracked file via `git ls-files`, skips non-UTF-8 / large
files, and reports findings. This is what CI runs.

### Staged diff (pre-commit)

```bash
cargo xtask check-secrets --staged
```

Reads the staged diff and reports findings only on lines the commit
will *add*. Use this as a manual preflight, or wire it into a git
pre-commit hook:

```bash
# .git/hooks/pre-commit  (chmod +x)
#!/usr/bin/env bash
exec cargo xtask check-secrets --staged
```

Suppression markers in the file work the same way they do in a repo
sweep (file-level and contiguous-comment-block walk-back both apply
to the staged blob).

### Stdin (pre-issue)

```bash
gh issue create --body "$(cat draft.md)"   # only AFTER:
cargo xtask check-secrets --stdin < draft.md
```

GitHub cannot run a true pre-issue / pre-PR-body hook. The practical
workflow is: draft the issue body in a local file, pipe it through the
scanner, then post. The scanner reports findings as `<stdin>:<line>:`
so you can locate them in your draft.

Always run this against any text you intend to paste into a public
issue, PR description, PR review comment, or chat channel.

## Allowlist (suppression) markers

Both markers must include the trailing **colon** and a one-line
rationale. The colon is what distinguishes the line marker from the
file marker, so a file-level marker on a comment line above a secret
does not accidentally fire the line-level walk-back. Reviewers should
reject suppressions that lack a rationale.

### Line marker — `djogi-allow-secret:`

Applies to the line it lives on, **and** to any line whose *contiguous
comment block immediately above* contains the marker. The walk-back is
capped at 20 lines and is terminated by any non-comment / blank line —
which keeps multi-line rationale comments working without bleeding into
unrelated code.

Recognised comment prefixes (after `trim_start`): `//`, `/*`, `*`,
`#`, `;`, `<!--`. That covers Rust, JS, Go, Python, shell, YAML, TOML,
Ruby, Markdown HTML comments, and Lisp/INI.

```yaml
# .github/workflows/ci.yml
        env:
          POSTGRES_USER: djogi
          # djogi-allow-secret: GHA service-container fixture; this Postgres
          # binds only to the runner-local network and is destroyed at job end.
          POSTGRES_PASSWORD: djogi
```

```rust
// djogi-allow-secret: synthetic admin URL used to exercise userinfo
// stripping; `secret` is a placeholder password.
let url = build_non_superuser_url(
    "postgres://admin:secret@db.local:5432/main",
    "djogi_test_001",
)?;
```

### File marker — `djogi-allow-secret-file:`

Applies to the entire file when present in the first 20 lines. Use
this only for files that are top-to-bottom fixtures — local dev
docker-compose, example seed files, scanner self-tests.

```yaml
# djogi-allow-secret-file: local dev cluster fixture; the `djogi:djogi`
# credentials are intentionally weak and never accept remote connections.
services:
  postgres:
    image: postgis/postgis:18-3.6
    environment:
      POSTGRES_USER: djogi
      POSTGRES_PASSWORD: djogi
```

## Built-in placeholder forms

Values containing any of the following are treated as obvious
placeholders and never flagged:

- `<…>` (angle-bracket placeholders, e.g. `<password>`, `<your-token>`)
- `${…}` (shell parameter expansion, e.g. `${VAULT_PASSWORD}`)
- `{{…}}` (template / mustache placeholders)
- `***`, `REDACTED`, `redacted`, `placeholder`, `xxx`
- `CHANGEME` / `changeme` / `Changeme`
- `your_`, `your-`, `YOUR_` prefixes
- `example.com` / `example.org` hosts
- `supersecretpassword` (explicit doc-example marker)

So you can write `postgres://<user>:<password>@<host>:<port>/<database>`
or `DATABASE_URL=${MY_VAULT_URL}` freely in public docs without a marker.

## Known intentionally-dummy local credentials

A small allowlist of "obvious local-dev fixture" `user:password` pairs
short-circuits the URL scanner. The current list is:

- `djogi:djogi`
- `postgres:postgres`
- `user:password`
- `user:pass`

These pairs are only allowed in URLs (e.g. `postgres://djogi:djogi@localhost`).
A bare `POSTGRES_PASSWORD: djogi` line is still flagged by the
env-assignment scanner and requires an explicit marker — this is
intentional, because the YAML / shell shape can travel into production
configuration without a port-mapped local host nearby.

## What to do when the scanner fires

1. **First — do not echo the value in chat, PR comments, or issue bodies.**
   The scanner output is already redacted; do not re-paste the original
   raw line.
2. Identify the file and line from the scanner output.
3. If the value is a real secret:
   - Remove it from the working tree.
   - Rotate the credential (treat it as compromised — it lived in your
     local checkout, your editor history, and your shell scrollback).
   - File a security advisory if it landed on a public branch. See
     [SECURITY.md](../../SECURITY.md).
4. If the value is an intentional fixture or anti-pattern example:
   - Add a `djogi-allow-secret:` marker on the line above, with a
     one-line rationale.
   - Re-run `cargo xtask check-secrets` to confirm the finding clears.

## Extending the scanner

Adding a new env-var to the named-secret list, or a new credential
URL scheme, is a one-line change in
`xtask/src/check_secrets.rs`. Keep the const tables sorted in ASCII
order — the compile-time guard panics if the invariant breaks.

Each new pattern should ship with at least one positive test (the
pattern fires) and one negative test (a placeholder form is allowed).
The scanner tests live under `#[cfg(test)] mod tests` in the same
file; run them with `cargo test -p xtask check_secrets`.

Patterns that need to fire on a value-only signal (e.g. AWS access
keys, which have a recognisable prefix and length) belong in a separate
pattern bucket — open an issue describing the shape and the test cases
before extending the scanner.
