// Polish — `DjogiContext::ensure_enum_type` live coverage.
//
// Closes GH issue #23 ("Helper for idempotent CREATE TYPE").
//
// # What this test pins
//
// 1. The helper successfully creates a Postgres enum type with the
//  requested variants (single round-trip).
// 2. Re-invoking with the same name is a no-op — the inner
//  `CREATE TYPE` raises `42710 duplicate_object`, the `EXCEPTION`
//  arm catches it, the outer `DO` block returns successfully. This
//  is the headline "idempotent" property the issue asked for.
// 3. Invalid-identifier names are rejected at the framework layer
//  before any SQL hits the database — empty string, leading digit,
//  non-ASCII byte, oversized.
// 4. Empty `variants` is rejected (Postgres requires at least one
//  label).

#[djogi::djogi_test]
async fn ensure_enum_type_creates_and_is_idempotent(mut ctx: djogi::DjogiContext) {
    let unique_name = format!("djogi_test_color_{}", std::process::id());

    // First call creates the type.
    ctx.ensure_enum_type(&unique_name, &["red", "green", "blue"])
        .await
        .expect("first ensure_enum_type creates the type");

    // Second call with the same name is a no-op (does not raise).
    ctx.ensure_enum_type(&unique_name, &["red", "green", "blue"])
        .await
        .expect("second ensure_enum_type is idempotent");

    // Third call with the same name but different variants is also a
    // no-op — the helper does not evolve variant lists; that's a real
    // migration's job. The duplicate_object branch fires regardless.
    ctx.ensure_enum_type(&unique_name, &["totally", "different"])
        .await
        .expect("re-issue with different variants stays a no-op");
}

#[djogi::djogi_test]
async fn ensure_enum_type_rejects_invalid_names(mut ctx: djogi::DjogiContext) {
    let cases = [
        ("", "cannot be empty"),
        ("9color", "ASCII letter or underscore"),
        (
            "color-with-hyphen",
            "not a valid unquoted Postgres identifier byte",
        ),
        ("café", "not a valid unquoted Postgres identifier byte"),
    ];
    for (name, expected_substr) in cases {
        let err = ctx
            .ensure_enum_type(name, &["a"])
            .await
            .expect_err("ensure_enum_type must reject invalid name");
        assert!(
            err.to_string().contains(expected_substr),
            "for name {name:?} expected substring {expected_substr:?} in error; got: {err}",
        );
    }

    // Oversized name (64 bytes) — Postgres NAMEDATALEN-1 is 63.
    let oversized: String = "a".repeat(64);
    let err = ctx
        .ensure_enum_type(&oversized, &["a"])
        .await
        .expect_err("ensure_enum_type must reject oversized name");
    assert!(
        err.to_string().contains("63 bytes"),
        "oversized name must surface NAMEDATALEN guidance; got: {err}",
    );
}

#[djogi::djogi_test]
async fn ensure_enum_type_rejects_empty_variants(mut ctx: djogi::DjogiContext) {
    let err = ctx
        .ensure_enum_type("djogi_test_empty_variants_enum", &[])
        .await
        .expect_err("ensure_enum_type must reject empty variants");
    assert!(
        err.to_string().contains("at least one variant"),
        "expected empty-variants guidance; got: {err}",
    );
}

#[djogi::djogi_test]
async fn ensure_enum_type_handles_quote_doubling_safely(mut ctx: djogi::DjogiContext) {
    // Defense-in-depth: variants that don't contain `$` but DO carry
    // SQL-metacharacter sequences (`'`, `;`, `--`, `/* ... */`) must
    // still round-trip safely. The single-quote doubling path and the
    // single-quoted-literal context handle them; nothing escapes the
    // DO body. This pins the safety claim an internal review flagged in the
    // re-review pass.
    let unique_name = format!("djogi_test_quote_doubling_{}", std::process::id());

    let safe_but_scary = [
        "a'b",                // embedded single quote
        "x; DROP TYPE t; --", // statement-terminator + line comment
        "/*comment*/",        // block comment marker
        "weird\"label",       // double quote (not a SQL identifier metacharacter inside literal)
    ];

    ctx.ensure_enum_type(&unique_name, &safe_but_scary)
        .await
        .expect("variants without `$` must round-trip even when scary-looking");
}

#[djogi::djogi_test]
async fn ensure_enum_type_rejects_dollar_in_variants(mut ctx: djogi::DjogiContext) {
    // Phase-boundary internal review (heeranjid#30 review pass) caught
    // that the prior bare-`$$` DO block had a SQL-injection surface:
    // a variant containing `$$` would close the dollar-quoted body
    // early because Postgres's lexer scans dollar-quoted strings for
    // the next matching tag regardless of intervening single-quote
    // context. The fix is two-layered: tagged dollar-quote
    // (`$djogi_ensure_enum$`) AND per-variant `$` rejection. This
    // test pins the rejection layer.
    for hostile in [
        "foo$$bar",
        "$",
        "x$djogi_ensure_enum$y",
        // even an innocent-looking `$` is rejected; callers who need
        // it must use a purpose-built migration path with its own escape regime.
        "tier$1",
    ] {
        let err = ctx
            .ensure_enum_type("djogi_test_dollar_variants", &[hostile])
            .await
            .expect_err("variants containing `$` must be rejected");
        assert!(
            err.to_string().contains("contains `$`"),
            "for variant {hostile:?} expected `$`-rejection guidance; got: {err}",
        );
    }
}
