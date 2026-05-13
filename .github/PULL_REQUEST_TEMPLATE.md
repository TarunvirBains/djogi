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
- [ ] (if macro changes) `cargo lihaaf --manifest-path djogi-macros/Cargo.toml -j 4` full sweep;
      if diagnostics changed, re-bless and commit:
      `cargo lihaaf --manifest-path djogi-macros/Cargo.toml --filter compile_fail --bless -j 4`

## Linked issue

Closes #
