# Security Policy

## Reporting a Vulnerability

If you discover a security vulnerability in djogi, please report
it through GitHub's [private security advisory][advisory]
mechanism rather than opening a public issue.

[advisory]: https://github.com/TarunvirBains/djogi/security/advisories/new

Include:

- A description of the issue and its impact
- Steps to reproduce, or a proof-of-concept where applicable
- The version (commit SHA or release tag) that's affected
- Any mitigations you've already explored

## Response timeline

This is a single-maintainer project pre-v1.0; response times are
best-effort. We aim to acknowledge reports within 7 days and
resolve confirmed issues within 30 days, though particularly
complex or low-impact issues may take longer.

## Supported versions

Pre-v1.0, only the `main` branch and the most recent published
release receive security fixes. Once v1.0 ships, supported
version ranges will be documented per release.

## Scope

In scope:

- The djogi framework crates (`djogi`, `djogi-macros`,
 `djogi-cli`, `djogi-shell`)
- The planned `djogi-maahi` admin console once its crate and feature
 flag ship; it is not a v0.1.0-alpha shipped surface
- Generated migration SQL — escape, injection, and
 least-privilege concerns
- Auth substrate (`PasswordHash`, `AuthContext`, RLS policy
 generation)

Out of scope:

- Vulnerabilities in transitive dependencies — please report to
 the upstream project; we'll bump the dep version once a fix
 ships
- Issues in adopter applications built with djogi — those are
 the application's responsibility, not the framework's
