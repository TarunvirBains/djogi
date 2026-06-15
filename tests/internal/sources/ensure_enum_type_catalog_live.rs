// Internal catalog coverage for `DjogiContext::ensure_enum_type`.
//
// The ordinary integration target covers the public helper behavior. These
// probes read Postgres enum catalogs directly to preserve the original checks
// that idempotent re-issues do not mutate label order and that scary labels
// are stored verbatim.

#[djogi::djogi_test]
async fn ensure_enum_type_catalog_preserves_original_label_order(mut ctx: djogi::DjogiContext) {
  let unique_name = format!("djogi_test_color_catalog_{}", std::process::id());

  ctx.ensure_enum_type(&unique_name, &["red", "green", "blue"])
    .await
    .expect("first ensure_enum_type creates the type");
  ctx.ensure_enum_type(&unique_name, &["totally", "different"])
    .await
    .expect("re-issue with different variants stays a no-op");

  let labels: String = ctx
    .raw_scalar(
      &format!("SELECT array_to_string(enum_range(NULL::{unique_name}), ',') AS labels"),
      &[],
    )
    .await
    .expect("read enum_range");
  let labels: Vec<&str> = labels.split(',').collect();

  assert_eq!(
    labels,
    vec!["red", "green", "blue"],
    "first call's variant order must persist; idempotent re-issues must not mutate it",
  );

  ctx.raw_execute(&format!("DROP TYPE {unique_name}"), &[])
    .await
    .expect("drop test enum type");
}

#[djogi::djogi_test]
async fn ensure_enum_type_catalog_stores_quote_doubled_labels_verbatim(
  mut ctx: djogi::DjogiContext,
) {
  let unique_name = format!("djogi_test_quote_doubling_catalog_{}", std::process::id());
  let safe_but_scary = [
    "a'b",
    "x; DROP TYPE t; --",
    "/*comment*/",
    "weird\"label",
  ];

  ctx.ensure_enum_type(&unique_name, &safe_but_scary)
    .await
    .expect("variants without `$` must round-trip even when scary-looking");

  let labels: String = ctx
    .raw_scalar(
      &format!("SELECT array_to_string(enum_range(NULL::{unique_name}), '|') AS labels"),
      &[],
    )
    .await
    .expect("read enum_range");
  let stored: Vec<&str> = labels.split('|').collect();

  assert_eq!(stored, safe_but_scary, "labels must round-trip verbatim");

  ctx.raw_execute(&format!("DROP TYPE {unique_name}"), &[])
    .await
    .expect("drop test enum type");
}
