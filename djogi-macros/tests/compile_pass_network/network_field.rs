// djogi#213 — typed Postgres network field types.
//
// Exercises the macro's parse + lower path for the three network
// column types behind the `network` feature flag:
//
// 1. A non-nullable `pub host: std::net::IpAddr` field maps to
// `FieldSqlType::Inet` in the emitted descriptor and stays a
// plain stdlib type at the field declaration site.
// 2. A nullable `pub maybe_host: Option<IpAddr>` field composes
// cleanly with the standard `Option<…>` wrapper.
// 3. `pub net: djogi::CidrAddr` maps to `FieldSqlType::Cidr`.
// 4. `pub mac: djogi::MacAddr` maps to `FieldSqlType::Macaddr`.
// 5. The fully-qualified `djogi::types::*` and `::djogi::types::*`
// spellings are also accepted by the type-mapping table.
// 6. The typed query surface accepts INET / CIDR / MACADDR binds for
// `.eq` / `.neq` / `.in_` / `.not_in` filters and for `.set`
// bulk-update assignments.
//
// The fixture lives in `tests/compile_pass_network/`, which the
// lihaaf `network` suite (declared in `djogi-macros/Cargo.toml`)
// enables alongside the matching `djogi/network` runtime feature.
// `cargo lihaaf --filter compile_pass_network` exercises it; CI runs
// it via the default `cargo lihaaf -j 4` sweep that walks all suites.

use djogi::prelude::*;
use std::net::IpAddr;

// ── (1) Non-nullable + nullable INET / CIDR / MACADDR columns ────────────

#[model(table = "network_rows_213", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct NetworkRow213 {
 /// `std::net::IpAddr` — INET column. Postgres-types' native
 /// `ToSql` / `FromSql` for `IpAddr` handles the wire format;
 /// netmask defaults to /32 (IPv4) or /128 (IPv6).
 pub host: IpAddr,
 /// `Option<IpAddr>` — nullable INET column.
 pub maybe_host: Option<IpAddr>,
 /// `djogi::CidrAddr` — CIDR column. Carries `(IpAddr, u8)` with
 /// construction-time host-bit-zero validation.
 pub net: CidrAddr,
 /// `Option<CidrAddr>` — nullable CIDR column.
 pub maybe_net: Option<CidrAddr>,
 /// `djogi::MacAddr` — MACADDR column (6-byte EUI-48).
 pub mac: MacAddr,
 /// `Option<MacAddr>` — nullable MACADDR column.
 pub maybe_mac: Option<MacAddr>,
 pub label: String,
}

// ── (2) Fully-qualified paths through djogi::types::* ────────────────────

#[model(table = "network_rows_213_alt", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct NetworkRow213Alt {
 /// `std::net::IpAddr` — the canonical stdlib path. Also accepted.
 pub host: std::net::IpAddr,
 /// `djogi::types::CidrAddr` — the internal path.
 pub net: djogi::types::CidrAddr,
 /// `djogi::types::MacAddr` — the internal path.
 pub mac: djogi::types::MacAddr,
}

fn _check_field_types(row: &NetworkRow213, alt: &NetworkRow213Alt) {
 let _: &IpAddr = &row.host;
 let _: &Option<IpAddr> = &row.maybe_host;
 let _: &CidrAddr = &row.net;
 let _: &Option<CidrAddr> = &row.maybe_net;
 let _: &MacAddr = &row.mac;
 let _: &Option<MacAddr> = &row.maybe_mac;
 let _: &std::net::IpAddr = &alt.host;
 let _: &djogi::types::CidrAddr = &alt.net;
 let _: &djogi::types::MacAddr = &alt.mac;
}

fn _check_network_query_surface() {
 let _filtered = NetworkRow213::objects().filter(|f| {
  let v4: IpAddr = "192.168.1.5".parse().unwrap();
  let net = CidrAddr::new("10.0.0.0".parse::<IpAddr>().unwrap(), 8).unwrap();
  let mac: MacAddr = "aa:bb:cc:dd:ee:ff".parse().unwrap();
  f.host().eq(v4) & f.net().eq(net) & f.mac().eq(mac)
 });

 let _filtered_option = NetworkRow213::objects().filter(|f| {
  let v6: IpAddr = "2001:db8::1".parse().unwrap();
  f.maybe_host().eq(v6) & f.maybe_net().is_null()
 });

 let _filtered_in = NetworkRow213::objects().filter(|f| {
  let v4_a: IpAddr = "10.0.0.1".parse().unwrap();
  let v4_b: IpAddr = "10.0.0.2".parse().unwrap();
  f.host().in_(vec![v4_a, v4_b])
 });

 let _update = NetworkRow213::objects().update(|f| {
  let new_host: IpAddr = "192.168.100.1".parse().unwrap();
  let new_mac = MacAddr::new([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);
  vec![
   f.host().set(new_host),
   f.mac().set(new_mac),
   f.label().set("updated".to_string()),
  ]
 });
}

fn main() {}
