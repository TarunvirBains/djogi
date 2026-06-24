// djogi#308 — InetAddr compile-pass fixture.
//
// Exercises the macro's parse + lower path for `djogi::InetAddr`,
// proving the classifier correctly maps `InetAddr` → INET:
//
// 1. A non-nullable `pub host: djogi::InetAddr` field maps to
//    `FieldSqlType::Inet` in the emitted descriptor.
// 2. A nullable `pub maybe_host: Option<InetAddr>` composes with
//    the standard `Option<…>` wrapper.
// 3. The fully-qualified `djogi::types::InetAddr` spelling is also
//    accepted by the type-mapping table.
//
// The fixture lives in `tests/compile_pass_network/`, which the lihaaf
// `network` suite enables alongside the matching `djogi/network` runtime
// feature.

use djogi::prelude::*;

#[model(table = "inet_addr_rows", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct InetAddrRow {
    /// `djogi::InetAddr` — INET column with explicit prefix length.
    pub host: InetAddr,
    /// `Option<InetAddr>` — nullable INET column.
    pub maybe_host: Option<InetAddr>,
    pub label: String,
}

#[model(table = "inet_addr_rows_alt", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct InetAddrRowAlt {
    /// Fully-qualified `djogi::types::InetAddr` path.
    pub host: djogi::types::InetAddr,
}

fn _check_field_types(row: &InetAddrRow, alt: &InetAddrRowAlt) {
    let _: &InetAddr = &row.host;
    let _: &Option<InetAddr> = &row.maybe_host;
    let _: &djogi::types::InetAddr = &alt.host;
}

fn main() {}
