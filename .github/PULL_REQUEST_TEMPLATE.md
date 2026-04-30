## Summary

<2–4 bullets — what ships, what's notable>

-
-

## Test plan

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --all-targets --all-features -- -D warnings`
- [ ] `cargo test --workspace -- --test-threads=1`
- [ ] (if data-layer changes) integration tests against live
      Postgres 18+
- [ ] (if macro changes) `cargo test -p djogi-macros --test
      trybuild_tests` full sweep

## Linked issue

Closes #
