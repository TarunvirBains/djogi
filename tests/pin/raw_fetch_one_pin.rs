struct RawFetchOneProbe {
    value: i32,
}

impl djogi::FromPgRow for RawFetchOneProbe {
    const COLUMNS: &'static [&'static str] = &["value"];
    const COLUMN_LIST: &'static str = "value";

    fn from_pg_row(row: &tokio_postgres::Row) -> Result<Self, djogi::DjogiError> {
        Ok(Self {
            value: row
                .try_get("value")
                .map_err(|e| djogi::DjogiError::Decode(format!("column `value`: {e}")))?,
        })
    }
}

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (PIN): exercises raw_fetch_one itself
#[djogi::djogi_test]
async fn raw_fetch_one_decodes_one_row(mut ctx: djogi::DjogiContext) {
    let row = ctx
        .raw_fetch_one::<RawFetchOneProbe>("SELECT $1::integer AS value", &[&43_i32])
        .await
        .expect("raw_fetch_one should decode exactly one row");

    assert_eq!(row.value, 43);
}
