// djogi#213 — typed Postgres network family
// (INET / CIDR / MACADDR) round-trip via the `network` Cargo feature.
//
// # What this file pins
//
// 1. **Descriptor projection.** Model fields typed `std::net::IpAddr` /
//    `djogi::CidrAddr` / `djogi::MacAddr` lower to
//    `FieldSqlType::Inet` / `Cidr` / `Macaddr` respectively, and the
//    migration composer emits `INET` / `CIDR` / `MACADDR` in the
//    column-type slot of `CREATE TABLE`.
// 2. **Wire-format round-trip.** Rows whose network columns carry IPv4
//    and IPv6 addresses, CIDR networks at various prefix lengths, and
//    EUI-48 MAC addresses round-trip end-to-end through `Model::create`
//    → `RETURNING *` → `FromPgRow` decode, preserving every byte.
// 3. **Nullability composition.** `Option<IpAddr>` / `Option<CidrAddr>`
//    / `Option<MacAddr>` columns round-trip both `None` (SQL NULL) and
//    `Some(...)` (typed value) through the framework's standard
//    nullable-column path.
// 4. **Filter execution.** `QuerySet::filter(|f| f.host().eq(...))`
//    on each typed network column emits a correctly-typed bind for the
//    matching Postgres type and returns only the matching row — pins
//    the `FilterValue::Inet` / `Cidr` / `Macaddr` → `push_bind` paths
//    at SQL-execution level.
// 5. **Bulk-update execution.** `QuerySet::update(|f| f.host().set(...))`
//    emits the correct `SET host = $1` clause executing through the
//    same `push_bind` path.
// 6. **Construction-time validation for CidrAddr.** `CidrAddr::new`
//    rejects host-bits-non-zero combinations before the wire codec
//    runs; the integration test pins both the accepted shapes
//    (`192.168.1.0/24`, `10.0.0.0/8`, `2001:db8::/32`) and that
//    construction-time validation runs before any DB round-trip.
//
// # No raw_execute required
//
// Every value the test inserts is reachable through the typed Rust
// surface (IpAddr / CidrAddr::new / MacAddr::new), so this file lives
// under `tests/integration/` (the raw-free integration target).

#![cfg(feature = "network")]

use djogi::prelude::*;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

// ── Test model — one column per network type, plus nullable counterparts ──

#[model(table = "network_rows", pk = HeerId, no_default)]
#[derive(Debug, Clone, PartialEq)]
pub struct NetworkRow {
    pub host: IpAddr,
    pub maybe_host: Option<IpAddr>,
    pub net: CidrAddr,
    pub maybe_net: Option<CidrAddr>,
    pub mac: MacAddr,
    pub maybe_mac: Option<MacAddr>,
    pub label: String,
}

// ── Round-trip — IPv4 inet, IPv4 cidr, EUI-48 macaddr ───────────────────────

#[djogi::djogi_test(sync_models = [NetworkRow])]
async fn network_ipv4_round_trip(mut ctx: djogi::DjogiContext) {
    let host = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5));
    let net = CidrAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 0)), 8).expect("valid CIDR");
    let mac = MacAddr::new([0x01, 0x23, 0x45, 0x67, 0x89, 0xab]);

    let row = NetworkRow::create(
        &mut ctx,
        NetworkRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            host,
            maybe_host: Some(host),
            net,
            maybe_net: Some(net),
            mac,
            maybe_mac: Some(mac),
            label: "ipv4-row".into(),
        },
    )
    .await
    .expect("IPv4 network columns must round-trip through Model::create");

    assert_eq!(row.host, host);
    assert_eq!(row.maybe_host, Some(host));
    assert_eq!(row.net, net);
    assert_eq!(row.maybe_net, Some(net));
    assert_eq!(row.mac, mac);
    assert_eq!(row.maybe_mac, Some(mac));

    // Re-fetch through Model::get to exercise the full decode path.
    let fetched = NetworkRow::get(&mut ctx, row.id)
        .await
        .expect("Model::get round-trip");
    assert_eq!(fetched.host, host);
    assert_eq!(fetched.net, net);
    assert_eq!(fetched.mac, mac);
}

// ── Round-trip — IPv6 ───────────────────────────────────────────────────────

#[djogi::djogi_test(sync_models = [NetworkRow])]
async fn network_ipv6_round_trip(mut ctx: djogi::DjogiContext) {
    // 2001:db8::1 — a unicast IPv6 host address. The /128 prefix is
    // implicit in IpAddr (host-address case).
    let host = IpAddr::V6("2001:db8::1".parse::<Ipv6Addr>().unwrap());
    // 2001:db8::/32 — the IETF documentation prefix. /32 is the
    // standard documentation-block prefix length.
    let net = CidrAddr::new(IpAddr::V6("2001:db8::".parse::<Ipv6Addr>().unwrap()), 32)
        .expect("valid CIDR");
    let mac = MacAddr::new([0xde, 0xad, 0xbe, 0xef, 0xca, 0xfe]);

    let row = NetworkRow::create(
        &mut ctx,
        NetworkRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            host,
            maybe_host: None,
            net,
            maybe_net: None,
            mac,
            maybe_mac: None,
            label: "ipv6-row".into(),
        },
    )
    .await
    .expect("IPv6 network columns must round-trip through Model::create");

    assert_eq!(row.host, host);
    assert_eq!(row.net, net);
    assert_eq!(row.mac, mac);

    let fetched = NetworkRow::get(&mut ctx, row.id)
        .await
        .expect("Model::get round-trip");
    assert_eq!(fetched.host, host);
    assert_eq!(fetched.net, net);
    assert_eq!(fetched.mac, mac);
}

// ── Round-trip — nullable columns with None ─────────────────────────────────

#[djogi::djogi_test(sync_models = [NetworkRow])]
async fn network_null_round_trips_as_none(mut ctx: djogi::DjogiContext) {
    let host = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
    let net = CidrAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0).expect("valid 0.0.0.0/0");
    let mac = MacAddr::default();

    let row = NetworkRow::create(
        &mut ctx,
        NetworkRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            host,
            maybe_host: None,
            net,
            maybe_net: None,
            mac,
            maybe_mac: None,
            label: "null-row".into(),
        },
    )
    .await
    .expect("nullable network columns with None must round-trip");
    assert_eq!(row.maybe_host, None);
    assert_eq!(row.maybe_net, None);
    assert_eq!(row.maybe_mac, None);

    let fetched = NetworkRow::get(&mut ctx, row.id)
        .await
        .expect("Model::get on nullable network row");
    assert_eq!(fetched.maybe_host, None);
    assert_eq!(fetched.maybe_net, None);
    assert_eq!(fetched.maybe_mac, None);
}

// ── Construction-time validation — CidrAddr ─────────────────────────────────

#[test]
fn cidraddr_construction_rejects_host_bits_set_before_db_round_trip() {
    // 192.168.1.5/24 — `.5` falls in the host portion of the /24
    // network. Postgres CIDR rejects this; our `CidrAddr::new`
    // catches it client-side so the framework surface never lets a
    // malformed value reach the wire codec.
    let addr = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5));
    assert!(CidrAddr::new(addr, 24).is_err());
}

#[test]
fn cidraddr_construction_rejects_oversized_prefix() {
    let addr = IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0));
    assert!(CidrAddr::new(addr, 33).is_err());
    let addr = IpAddr::V6(Ipv6Addr::UNSPECIFIED);
    assert!(CidrAddr::new(addr, 129).is_err());
}

// ── Runtime filter execution — INET, CIDR, MACADDR ──────────────────────────

#[djogi::djogi_test(sync_models = [NetworkRow])]
async fn network_filter_by_inet_returns_matching_row(mut ctx: djogi::DjogiContext) {
    let target_host = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
    let other_host = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2));
    let net = CidrAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0).unwrap();
    let mac = MacAddr::default();

    for (host, label) in [(target_host, "filter-target"), (other_host, "filter-other")] {
        NetworkRow::create(
            &mut ctx,
            NetworkRow {
                id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
                created_at: ::djogi::types::DateTime::UNIX_EPOCH,
                updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
                host,
                maybe_host: None,
                net,
                maybe_net: None,
                mac,
                maybe_mac: None,
                label: label.into(),
            },
        )
        .await
        .expect("create row for INET filter");
    }

    let results = NetworkRow::objects()
        .filter(|f| f.host().eq(target_host))
        .fetch_all(&mut ctx)
        .await
        .expect("filter by INET eq must execute");

    assert_eq!(results.len(), 1, "filter should return exactly one row");
    assert_eq!(results[0].label, "filter-target");
    assert_eq!(results[0].host, target_host);
}

#[djogi::djogi_test(sync_models = [NetworkRow])]
async fn network_filter_by_cidr_returns_matching_row(mut ctx: djogi::DjogiContext) {
    let target_net = CidrAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 0)), 8).unwrap();
    let other_net = CidrAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 0, 0)), 16).unwrap();
    let host = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
    let mac = MacAddr::default();

    for (net, label) in [(target_net, "cidr-target"), (other_net, "cidr-other")] {
        NetworkRow::create(
            &mut ctx,
            NetworkRow {
                id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
                created_at: ::djogi::types::DateTime::UNIX_EPOCH,
                updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
                host,
                maybe_host: None,
                net,
                maybe_net: None,
                mac,
                maybe_mac: None,
                label: label.into(),
            },
        )
        .await
        .expect("create row for CIDR filter");
    }

    let results = NetworkRow::objects()
        .filter(|f| f.net().eq(target_net))
        .fetch_all(&mut ctx)
        .await
        .expect("filter by CIDR eq must execute");

    assert_eq!(results.len(), 1, "filter should return exactly one row");
    assert_eq!(results[0].label, "cidr-target");
    assert_eq!(results[0].net, target_net);
}

#[djogi::djogi_test(sync_models = [NetworkRow])]
async fn network_filter_by_macaddr_returns_matching_row(mut ctx: djogi::DjogiContext) {
    let target_mac = MacAddr::new([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);
    let other_mac = MacAddr::new([0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
    let host = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
    let net = CidrAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0).unwrap();

    for (mac, label) in [(target_mac, "mac-target"), (other_mac, "mac-other")] {
        NetworkRow::create(
            &mut ctx,
            NetworkRow {
                id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
                created_at: ::djogi::types::DateTime::UNIX_EPOCH,
                updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
                host,
                maybe_host: None,
                net,
                maybe_net: None,
                mac,
                maybe_mac: None,
                label: label.into(),
            },
        )
        .await
        .expect("create row for MACADDR filter");
    }

    let results = NetworkRow::objects()
        .filter(|f| f.mac().eq(target_mac))
        .fetch_all(&mut ctx)
        .await
        .expect("filter by MACADDR eq must execute");

    assert_eq!(results.len(), 1, "filter should return exactly one row");
    assert_eq!(results[0].label, "mac-target");
    assert_eq!(results[0].mac, target_mac);
}

// ── Bulk-update execution — host SET ────────────────────────────────────────

#[djogi::djogi_test(sync_models = [NetworkRow])]
async fn network_bulk_update_sets_host(mut ctx: djogi::DjogiContext) {
    let initial_host = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
    let updated_host = IpAddr::V4(Ipv4Addr::new(192, 168, 100, 1));
    let net = CidrAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0).unwrap();
    let mac = MacAddr::default();

    let row = NetworkRow::create(
        &mut ctx,
        NetworkRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            host: initial_host,
            maybe_host: None,
            net,
            maybe_net: None,
            mac,
            maybe_mac: None,
            label: "bulk-update-host".into(),
        },
    )
    .await
    .expect("create row for bulk-update INET");

    let n = NetworkRow::objects()
        .filter(|f| f.host().eq(initial_host))
        .update(|f| f.host().set(updated_host))
        .execute(&mut ctx)
        .await
        .expect("bulk update of INET column must execute");

    assert_eq!(n, 1, "exactly one row should be updated");

    let fetched = NetworkRow::get(&mut ctx, row.id)
        .await
        .expect("re-fetch after bulk update");

    assert_eq!(
        fetched.host, updated_host,
        "host must reflect the bulk-updated value"
    );
}

// ── InetAddr round-trip — IPv4 host-with-prefix + nullable (#308 T14) ────────

#[model(table = "inet_addr_rows_test", pk = HeerId, no_default)]
#[derive(Debug, Clone, PartialEq)]
pub struct InetAddrRow {
    pub addr: InetAddr,
    pub maybe_addr: Option<InetAddr>,
    pub label: String,
}

#[djogi::djogi_test(sync_models = [InetAddrRow])]
async fn inet_addr_ipv4_host_with_prefix_round_trip(mut ctx: djogi::DjogiContext) {
    // 192.168.1.5/24 — a host address with a /24 prefix.
    // CidrAddr rejects this (host bits set), but InetAddr permits it.
    let inet =
        InetAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5)), 24).expect("valid InetAddr");

    let row = InetAddrRow::create(
        &mut ctx,
        InetAddrRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            addr: inet,
            maybe_addr: None,
            label: "inet-host-prefix".into(),
        },
    )
    .await
    .expect("InetAddr with host-with-prefix must round-trip");

    // Verify both address and prefix are preserved through the wire.
    assert_eq!(row.addr.addr(), IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5)));
    assert_eq!(row.addr.prefix(), 24);
    assert_eq!(row.maybe_addr, None);

    // Re-fetch through Model::get to exercise the full decode path.
    let fetched = InetAddrRow::get(&mut ctx, row.id)
        .await
        .expect("Model::get round-trip for InetAddr");
    assert_eq!(
        fetched.addr.addr(),
        IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5))
    );
    assert_eq!(fetched.addr.prefix(), 24);
    assert_eq!(fetched.maybe_addr, None);
}

// ── InetAddr IPv6 + filter execution (#308 T15) ──────────────────────────────

#[djogi::djogi_test(sync_models = [InetAddrRow])]
async fn inet_addr_ipv6_round_trip_and_filters(mut ctx: djogi::DjogiContext) {
    // 2001:db8::1/64 — IPv6 address with /64 prefix.
    let inet_v6 = InetAddr::new(IpAddr::V6("2001:db8::1".parse::<Ipv6Addr>().unwrap()), 64)
        .expect("valid IPv6 InetAddr");

    // 10.0.0.1/32 — IPv4 host for the in_/not_in_ tests.
    let inet_ipv4_1 = InetAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 32).expect("valid");

    // 10.0.0.2/32 — second IPv4 host.
    let inet_ipv4_2 = InetAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)), 32).expect("valid");

    // 10.0.0.3/32 — third IPv4 host (used in not_in exclusion).
    let inet_ipv4_3 = InetAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 3)), 32).expect("valid");

    for (addr, label) in [
        (inet_v6, "inet-v6"),
        (inet_ipv4_1, "inet-ipv4-1"),
        (inet_ipv4_2, "inet-ipv4-2"),
        (inet_ipv4_3, "inet-ipv4-3"),
    ] {
        InetAddrRow::create(
            &mut ctx,
            InetAddrRow {
                id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
                created_at: ::djogi::types::DateTime::UNIX_EPOCH,
                updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
                addr,
                maybe_addr: Some(addr),
                label: label.into(),
            },
        )
        .await
        .expect("create InetAddr row");
    }

    // Verify IPv6 round-trip and equality filter.
    let results = InetAddrRow::objects()
        .filter(|f| f.addr().eq(inet_v6))
        .fetch_all(&mut ctx)
        .await
        .expect("filter by InetAddr eq must execute");
    assert_eq!(
        results.len(),
        1,
        "IPv6 eq filter should return exactly one row"
    );
    assert_eq!(results[0].label, "inet-v6");
    assert_eq!(results[0].addr.addr(), inet_v6.addr());
    assert_eq!(results[0].addr.prefix(), inet_v6.prefix());

    // .in_() — returns rows matching any value in the list.
    let results = InetAddrRow::objects()
        .filter(|f| f.addr().in_([inet_ipv4_1, inet_ipv4_2]))
        .fetch_all(&mut ctx)
        .await
        .expect("filter by InetAddr in_ must execute");
    assert_eq!(results.len(), 2, "in_ should return two matching rows");
    let labels: Vec<_> = results.iter().map(|r| r.label.as_str()).collect();
    assert!(labels.contains(&"inet-ipv4-1"));
    assert!(labels.contains(&"inet-ipv4-2"));

    // .not_in() — excludes rows matching any value in the list.
    let results = InetAddrRow::objects()
        .filter(|f| f.addr().not_in([inet_ipv4_1, inet_ipv4_2]))
        .fetch_all(&mut ctx)
        .await
        .expect("filter by InetAddr not_in must execute");
    assert_eq!(
        results.len(),
        2,
        "not_in should return the two excluded rows (v6 + ipv4-3)"
    );
    let labels: Vec<_> = results.iter().map(|r| r.label.as_str()).collect();
    assert!(labels.contains(&"inet-v6"));
    assert!(labels.contains(&"inet-ipv4-3"));
}

// ── InetAddr containment operators (#308 T16) ────────────────────────────────

#[djogi::djogi_test(sync_models = [InetAddrRow])]
async fn inet_addr_containment_operators_execute(mut ctx: djogi::DjogiContext) {
    // 10.0.0.0/8 — wide network, contains most test addresses.
    let inet_wide = InetAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 0)), 8).expect("valid");

    // 192.168.1.0/24 — narrow network.
    let inet_narrow = InetAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 0)), 24).expect("valid");

    // 192.168.1.5/32 — host address within the /24 network.
    let inet_host = InetAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5)), 32).expect("valid");

    // 172.16.0.0/12 — another network, used for overlap test.
    let inet_other = InetAddr::new(IpAddr::V4(Ipv4Addr::new(172, 16, 0, 0)), 12).expect("valid");

    for (addr, label) in [
        (inet_wide, "inet-wide"),
        (inet_narrow, "inet-narrow"),
        (inet_host, "inet-host"),
        (inet_other, "inet-other"),
    ] {
        InetAddrRow::create(
            &mut ctx,
            InetAddrRow {
                id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
                created_at: ::djogi::types::DateTime::UNIX_EPOCH,
                updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
                addr,
                maybe_addr: None,
                label: label.into(),
            },
        )
        .await
        .expect("create InetAddr row for containment");
    }

    // contains (>>): 10.0.0.0/8 contains 192.168.1.5/32? No, different range.
    // But 192.168.1.0/24 contains 192.168.1.5/32? Yes.
    let results = InetAddrRow::objects()
        .filter(|f| f.addr().contains(inet_host))
        .fetch_all(&mut ctx)
        .await
        .expect("contains filter must execute");
    // Only 192.168.1.0/24 strictly contains 192.168.1.5/32.
    assert_eq!(results.len(), 1, "only /24 network should contain the host");
    assert_eq!(results[0].label, "inet-narrow");

    // contained_by (<<): 192.168.1.5/32 is contained by 10.0.0.0/8? No.
    // But 192.168.1.5/32 is contained by 192.168.1.0/24? Yes.
    // Test: which rows are contained by 10.0.0.0/8? Only the wide one itself (equal).
    let results = InetAddrRow::objects()
        .filter(|f| f.addr().contained_by(inet_wide))
        .fetch_all(&mut ctx)
        .await
        .expect("contained_by filter must execute");
    // 10.0.0.0/8 is contained by itself (<<= but not <<).
    // Actually << means "strictly contained by". 10.0.0.0/8 << 10.0.0.0/8 is false.
    assert_eq!(results.len(), 0, "nothing strictly contained by /8 here");

    // Test with a broader network: which rows are contained by 0.0.0.0/0?
    let inet_all = InetAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0).expect("valid");
    let results = InetAddrRow::objects()
        .filter(|f| f.addr().contained_by(inet_all))
        .fetch_all(&mut ctx)
        .await
        .expect("contained_by 0.0.0.0/0 must execute");
    // All four rows should be contained by 0.0.0.0/0.
    assert_eq!(results.len(), 4, "all rows contained by 0.0.0.0/0");

    // overlaps (&&): networks that overlap with 192.168.0.0/16
    let supernet = CidrAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 0, 0)), 16).expect("valid");
    let results = InetAddrRow::objects()
        .filter(|f| f.addr().overlaps(supernet))
        .fetch_all(&mut ctx)
        .await
        .expect("overlaps filter must execute");
    // 192.168.1.0/24 and 192.168.1.5/32 overlap with 192.168.0.0/16.
    assert_eq!(
        results.len(),
        2,
        "two rows should overlap with /16 supernet"
    );
    let labels: Vec<_> = results.iter().map(|r| r.label.as_str()).collect();
    assert!(labels.contains(&"inet-narrow"));
    assert!(labels.contains(&"inet-host"));
}

// ── InetAddr scalar functions execute (#496) ────────────────────────────────

#[djogi::djogi_test(sync_models = [InetAddrRow])]
async fn inet_scalar_functions_execute(mut ctx: djogi::DjogiContext) {
    use djogi::expr::Expr;

    // 192.168.1.5/24 — IPv4 host with a /24 prefix.
    let v4 = InetAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5)), 24).expect("valid");
    // 2001:db8::1/128 — IPv6 host.
    let v6 = InetAddr::new(
        IpAddr::V6("2001:db8::1".parse::<Ipv6Addr>().expect("valid v6")),
        128,
    )
    .expect("valid");

    for (addr, label) in [(v4, "v4"), (v6, "v6")] {
        InetAddrRow::create(
            &mut ctx,
            InetAddrRow {
                id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
                created_at: ::djogi::types::DateTime::UNIX_EPOCH,
                updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
                addr,
                maybe_addr: None,
                label: label.into(),
            },
        )
        .await
        .expect("create row");
    }

    // family(addr) = 4 → only the v4 row.
    let v4_rows = InetAddrRow::objects()
        .filter_expr(|f| f.addr().family().eq(Expr::literal(4i32)))
        .fetch_all(&mut ctx)
        .await
        .expect("family() predicate executes");
    assert_eq!(v4_rows.len(), 1, "exactly one IPv4 row");
    assert_eq!(v4_rows[0].label, "v4");

    // family(addr) = 6 → only the v6 row.
    let v6_rows = InetAddrRow::objects()
        .filter_expr(|f| f.addr().family().eq(Expr::literal(6i32)))
        .fetch_all(&mut ctx)
        .await
        .expect("family() predicate executes");
    assert_eq!(v6_rows.len(), 1, "exactly one IPv6 row");
    assert_eq!(v6_rows[0].label, "v6");

    // masklen(addr) = 24 → only the v4 row (prefix 24).
    let masklen_rows = InetAddrRow::objects()
        .filter_expr(|f| f.addr().masklen().eq(Expr::literal(24i32)))
        .fetch_all(&mut ctx)
        .await
        .expect("masklen() predicate executes");
    assert_eq!(masklen_rows.len(), 1, "one row with /24");
    assert_eq!(masklen_rows[0].label, "v4");
}

// ── set_masklen + arithmetic via set_expr (#496) ─────────────────────────────

#[djogi::djogi_test(sync_models = [InetAddrRow])]
async fn inet_set_masklen_and_arithmetic_execute(mut ctx: djogi::DjogiContext) {
    // 10.0.0.0/8.
    let base = InetAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 0)), 8).expect("valid");
    let row = InetAddrRow::create(
        &mut ctx,
        InetAddrRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            addr: base,
            maybe_addr: None,
            label: "base".into(),
        },
    )
    .await
    .expect("create base row");

    // UPDATE addr = set_masklen(addr, 16) — rewrite the prefix to /16.
    InetAddrRow::objects()
        .filter(|f| f.label().eq("base"))
        .update(|f| f.addr().set_expr(f.addr().set_masklen(16)))
        .execute(&mut ctx)
        .await
        .expect("set_masklen UPDATE executes");

    let fetched = InetAddrRow::get(&mut ctx, row.id).await.expect("re-fetch");
    assert_eq!(fetched.addr.prefix(), 16, "prefix rewritten to /16");
    assert_eq!(
        fetched.addr.addr(),
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, 0)),
        "address bits preserved by set_masklen"
    );

    // UPDATE addr = addr + 5 — add an offset to the address.
    InetAddrRow::objects()
        .filter(|f| f.label().eq("base"))
        .update(|f| f.addr().set_expr(f.addr().add_offset(5)))
        .execute(&mut ctx)
        .await
        .expect("add_offset UPDATE executes");

    let fetched = InetAddrRow::get(&mut ctx, row.id)
        .await
        .expect("re-fetch after add_offset");
    assert_eq!(
        fetched.addr.addr(),
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5)),
        "10.0.0.0 + 5 = 10.0.0.5"
    );
}

// ── network() (cidr-returning) + remaining INET surfaces execute (#496) ──────
//
// A dedicated model with BOTH an inet column and a cidr column. The cidr
// column is the ONLY consumption path for `network()`'s `Expr<CidrAddr>`
// result: it cannot be `set_expr`'d onto an `InetAddr` column (type
// mismatch) and cannot be compared via `.eq()` (no `CidrAddr` literal
// bridge exists in `expr/literal.rs`). Writing `network(addr)` into a real
// `cidr` column and decoding it back through `CidrAddr::FromSql` is the
// faithful end-to-end test of the cross-type return.
#[model(table = "inet_cidr_rows_test", pk = HeerId, no_default)]
#[derive(Debug, Clone, PartialEq)]
pub struct InetCidrRow {
    pub addr: InetAddr,
    pub net: CidrAddr,
    pub label: String,
}

#[djogi::djogi_test(sync_models = [InetCidrRow])]
async fn inet_network_and_remaining_functions_execute(mut ctx: djogi::DjogiContext) {
    use djogi::expr::Expr;

    // 192.168.1.5/24 — host with host-bits set; network() zeroes them to
    // 192.168.1.0/24. `net` is seeded as 0.0.0.0/0 (overwritten below).
    let addr = InetAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5)), 24).expect("valid");
    let seed_net = CidrAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0).expect("valid");
    let row = InetCidrRow::create(
        &mut ctx,
        InetCidrRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            addr,
            net: seed_net,
            label: "net".into(),
        },
    )
    .await
    .expect("create row");

    // 1. network(addr) -> cidr -> stored in the cidr column -> decoded back.
    //    UPDATE net = network(addr); network(192.168.1.5/24) = 192.168.1.0/24.
    //    `f.net()` is DjogiField<M, CidrAddr>, so set_expr wants Expr<CidrAddr>,
    //    which `f.addr().network()` returns — types match.
    InetCidrRow::objects()
        .filter(|f| f.label().eq("net"))
        .update(|f| f.net().set_expr(f.addr().network()))
        .execute(&mut ctx)
        .await
        .expect("network() UPDATE into cidr column executes");

    let fetched = InetCidrRow::get(&mut ctx, row.id)
        .await
        .expect("re-fetch after network()");
    assert_eq!(
        fetched.net.addr(),
        IpAddr::V4(Ipv4Addr::new(192, 168, 1, 0)),
        "network(192.168.1.5/24) zeroes host bits to 192.168.1.0"
    );
    assert_eq!(
        fetched.net.prefix(),
        24,
        "network() preserves the /24 prefix as a cidr"
    );

    // 2. broadcast(addr) -> inet -> stored in the inet column -> decoded.
    //    broadcast(192.168.1.5/24) = 192.168.1.255 (still /24).
    InetCidrRow::objects()
        .filter(|f| f.label().eq("net"))
        .update(|f| f.addr().set_expr(f.addr().broadcast()))
        .execute(&mut ctx)
        .await
        .expect("broadcast() UPDATE executes");

    let fetched = InetCidrRow::get(&mut ctx, row.id)
        .await
        .expect("re-fetch after broadcast()");
    assert_eq!(
        fetched.addr.addr(),
        IpAddr::V4(Ipv4Addr::new(192, 168, 1, 255)),
        "broadcast(192.168.1.5/24) = 192.168.1.255"
    );

    // 3. netmask(addr) -> inet. netmask(192.168.1.255/24) = 255.255.255.0.
    InetCidrRow::objects()
        .filter(|f| f.label().eq("net"))
        .update(|f| f.addr().set_expr(f.addr().netmask()))
        .execute(&mut ctx)
        .await
        .expect("netmask() UPDATE executes");

    let fetched = InetCidrRow::get(&mut ctx, row.id)
        .await
        .expect("re-fetch after netmask()");
    assert_eq!(
        fetched.addr.addr(),
        IpAddr::V4(Ipv4Addr::new(255, 255, 255, 0)),
        "netmask(/24) = 255.255.255.0"
    );

    // 4. hostmask(addr) -> inet. The address is now 255.255.255.0/32
    //    (netmask() returns a /32 host); hostmask(x/32) = 0.0.0.0.
    //    Assert execution + a decodable result, not an exact host value
    //    (the prefix carried by the prior result drives the mask).
    InetCidrRow::objects()
        .filter(|f| f.label().eq("net"))
        .update(|f| f.addr().set_expr(f.addr().hostmask()))
        .execute(&mut ctx)
        .await
        .expect("hostmask() UPDATE executes");
    let _ = InetCidrRow::get(&mut ctx, row.id)
        .await
        .expect("hostmask() result decodes as InetAddr");

    // Seed a second row dedicated to arithmetic so the known value is stable.
    let arith = InetAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 10)), 32).expect("valid");
    let arith_row = InetCidrRow::create(
        &mut ctx,
        InetCidrRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            addr: arith,
            net: seed_net,
            label: "arith".into(),
        },
    )
    .await
    .expect("create arith row");

    // 5. sub_offset(4): 10.0.0.10 - 4 = 10.0.0.6.
    InetCidrRow::objects()
        .filter(|f| f.label().eq("arith"))
        .update(|f| f.addr().set_expr(f.addr().sub_offset(4)))
        .execute(&mut ctx)
        .await
        .expect("sub_offset() UPDATE executes");
    let fetched = InetCidrRow::get(&mut ctx, arith_row.id)
        .await
        .expect("re-fetch after sub_offset()");
    assert_eq!(
        fetched.addr.addr(),
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, 6)),
        "10.0.0.10 - 4 = 10.0.0.6"
    );

    // 6a. bitnot() -> inet. Unary `~ inet`; column-bound, value-asserted.
    //     Reset addr to a known host first so the NOT is deterministic.
    //     ~'10.0.0.10/32' NOTs each octet: ~10=245, ~0=255, ~0=255, ~10=245
    //     → 245.255.255.245 (prefix /32 unchanged).
    InetCidrRow::objects()
        .filter(|f| f.label().eq("arith"))
        .update(|f| f.addr().set(arith)) // back to 10.0.0.10/32
        .execute(&mut ctx)
        .await
        .expect("reset addr for bitnot executes");
    InetCidrRow::objects()
        .filter(|f| f.label().eq("arith"))
        .update(|f| f.addr().set_expr(f.addr().bitnot()))
        .execute(&mut ctx)
        .await
        .expect("bitnot() UPDATE executes");
    let fetched = InetCidrRow::get(&mut ctx, arith_row.id)
        .await
        .expect("re-fetch after bitnot()");
    assert_eq!(
        fetched.addr.addr(),
        IpAddr::V4(Ipv4Addr::new(245, 255, 255, 245)),
        "~10.0.0.10 = 245.255.255.245 (bitwise NOT per octet)"
    );

    // 6b. bitand_addr(~addr): x & ~x = 0 for every bit, independent of prefix /
    //     version / family. Distinct operands (Field vs InetBitNot node) -> SQL
    //     `(addr & (~addr))`; an identity-returning miswiring would yield
    //     10.0.0.10, not 0.0.0.0. Reset addr to the known arith value first.
    InetCidrRow::objects()
        .filter(|f| f.label().eq("arith"))
        .update(|f| f.addr().set(arith)) // back to 10.0.0.10/32
        .execute(&mut ctx)
        .await
        .expect("reset addr for bitand executes");
    InetCidrRow::objects()
        .filter(|f| f.label().eq("arith"))
        .update(|f| f.addr().set_expr(f.addr().bitand_addr(f.addr().bitnot())))
        .execute(&mut ctx)
        .await
        .expect("bitand_addr() UPDATE executes");
    let fetched = InetCidrRow::get(&mut ctx, arith_row.id)
        .await
        .expect("re-fetch after bitand_addr()");
    assert_eq!(
        fetched.addr.addr(),
        IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
        "addr & ~addr = 0.0.0.0 (self-complement AND; distinguishes correct & from identity)"
    );

    // 6c. bitor_addr(~addr): x | ~x = all-ones, independent of prefix/version.
    //     6b leaves addr = 0.0.0.0, so reset to the known arith value first.
    InetCidrRow::objects()
        .filter(|f| f.label().eq("arith"))
        .update(|f| f.addr().set(arith)) // back to 10.0.0.10/32
        .execute(&mut ctx)
        .await
        .expect("reset addr for bitor executes");
    InetCidrRow::objects()
        .filter(|f| f.label().eq("arith"))
        .update(|f| f.addr().set_expr(f.addr().bitor_addr(f.addr().bitnot())))
        .execute(&mut ctx)
        .await
        .expect("bitor_addr() UPDATE executes");
    let fetched = InetCidrRow::get(&mut ctx, arith_row.id)
        .await
        .expect("re-fetch after bitor_addr()");
    assert_eq!(
        fetched.addr.addr(),
        IpAddr::V4(Ipv4Addr::new(255, 255, 255, 255)),
        "addr | ~addr = 255.255.255.255 (self-complement OR; distinguishes correct | from identity)"
    );

    // 7. sub_addr(other) -> bigint. addr - (addr - 5) = 5. Distinct operands
    //    (Field vs InetSubOffset); larger-minus-smaller is unambiguously +5
    //    regardless of PG's signed difference convention. A self-difference bug
    //    (returning 0) fails this. Reset addr to arith first (6c left it
    //    255.255.255.255).
    InetCidrRow::objects()
        .filter(|f| f.label().eq("arith"))
        .update(|f| f.addr().set(arith)) // back to 10.0.0.10/32
        .execute(&mut ctx)
        .await
        .expect("reset addr for sub_addr executes");
    let five_diff = InetCidrRow::objects()
        .filter(|f| f.label().eq("arith"))
        .filter_expr(|f| {
            f.addr()
                .sub_addr(f.addr().sub_offset(5))
                .eq(Expr::literal(5i64))
        })
        .fetch_all(&mut ctx)
        .await
        .expect("sub_addr() predicate executes");
    assert_eq!(
        five_diff.len(),
        1,
        "addr - (addr - 5) = 5 matches the arith row (distinct operands; not self-difference)"
    );

    // 8. abbrev(addr) / host(addr) -> text. These return Expr<String>; the
    //    bare `"..."` RHS goes through the From<&'static str> for Expr<String>
    //    bridge. addr is still 10.0.0.10/32 from the reset in step 7.
    //    host(addr) -> text, prefix ALWAYS stripped: host('10.0.0.10/32') =
    //    '10.0.0.10'. This format is unambiguous and documented, so it is
    //    value-asserted.
    let host_rows = InetCidrRow::objects()
        .filter(|f| f.label().eq("arith"))
        .filter_expr(|f| f.addr().host().eq("10.0.0.10"))
        .fetch_all(&mut ctx)
        .await
        .expect("host() predicate executes");
    assert_eq!(host_rows.len(), 1, "host(addr) = 10.0.0.10 matches");
    // abbrev(addr) -> text. The exact abbreviated form for a /32 inet is
    // Postgres-version-dependent (it may or may not retain the `/32`), so
    // this surface is exercised EXECUTION-ONLY: compose abbrev() into a
    // predicate that must run without a Postgres error, without asserting an
    // exact string. (host() above already value-asserts an INET text fn, and
    // the CIDR abbrev assertion below value-asserts the cidr overload.)
    let _abbrev_runs = InetCidrRow::objects()
        .filter(|f| f.label().eq("arith"))
        .filter_expr(|f| f.addr().abbrev().eq(f.addr().host()))
        .fetch_all(&mut ctx)
        .await
        .expect("abbrev() predicate executes without error");

    // 8b. abbrev(net) on a CidrAddr column — Postgres CIDR abbreviated form
    //     omits trailing zero octets: abbrev('192.168.1.0/24'::cidr) =
    //     '192.168.1/24'. The `net` column holds 192.168.1.0/24 from the
    //     network() round-trip in step 1. This is the definitive value
    //     assertion distinguishing CIDR abbrev output from INET abbrev output,
    //     and is deterministic / version-stable.
    let cidr_abbrev_rows = InetCidrRow::objects()
        .filter(|f| f.label().eq("net"))
        .filter_expr(|f| f.net().abbrev().eq("192.168.1/24"))
        .fetch_all(&mut ctx)
        .await
        .expect("CIDR abbrev() predicate executes");
    assert_eq!(
        cidr_abbrev_rows.len(),
        1,
        "abbrev(net::cidr) = '192.168.1/24' (CIDR trailing-zero-octet omission)"
    );

    // 9. inet_server_addr() / inet_server_port() — session functions. These
    //    may return NULL on a Unix-socket connection, so assert EXECUTION
    //    (the predicate runs without a Postgres error), not a specific row
    //    count. `inet_server_addr()` is Expr<InetAddr> with no literal bridge,
    //    so it cannot be a literal RHS — instead compare the COLUMN's expr to
    //    it: `f.addr().as_expr().eq(Expr::inet_server_addr())` (reflexive
    //    Into<Expr<InetAddr>>). `inet_server_port()` is Expr<i32> and pairs
    //    with an i32 literal.
    let _server_addr = InetCidrRow::objects()
        .filter_expr(|f| f.addr().as_expr().eq(Expr::inet_server_addr()))
        .fetch_all(&mut ctx)
        .await
        .expect("inet_server_addr() predicate executes without error");
    let _server_port = InetCidrRow::objects()
        .filter_expr(|_f| Expr::inet_server_port().eq(Expr::literal(5432i32)))
        .fetch_all(&mut ctx)
        .await
        .expect("inet_server_port() predicate executes without error");
}

// ── Nullable INET transform composes in filter_expr (#496) ───────────────────
//
// The nullable transform methods return `Expr<Option<T>>`, which composes into
// `filter_expr` only by comparison against another `Expr<Option<T>>` (no
// literal bridge exists for `Option<T>`). `set_expr` is NOT available on a
// nullable INET column — its path is `IntoFilterValue`-bounded and
// `Option<InetAddr>` is not `IntoFilterValue`; nullable-column expression
// UPDATE assignment is a separate, framework-wide follow-up. This test
// exercises the real (`filter_expr`) consumption path and asserts SQL
// three-valued logic end-to-end against live Postgres.
#[djogi::djogi_test(sync_models = [InetAddrRow])]
async fn inet_nullable_transform_composes_in_filter_expr(mut ctx: djogi::DjogiContext) {
    // 10.0.0.0/16 in the nullable column for the Some row.
    let addr = InetAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 0)), 16).expect("valid");

    let some_row = InetAddrRow::create(
        &mut ctx,
        InetAddrRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            addr,
            maybe_addr: Some(addr),
            label: "some".into(),
        },
    )
    .await
    .expect("create some row");

    let _none_row = InetAddrRow::create(
        &mut ctx,
        InetAddrRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            addr,
            maybe_addr: None,
            label: "none".into(),
        },
    )
    .await
    .expect("create none row");

    // The nullable transform returns Expr<Option<i32>>; it composes into
    // filter_expr only by comparison against another Expr<Option<i32>>
    // (no literal bridge exists for Option<T>). Self-comparison exercises
    // the nullable path AND asserts SQL three-valued logic: for the None
    // row, masklen(NULL) = NULL and (NULL = NULL) is NULL -> excluded;
    // for the Some row, masklen(x) = masklen(x) is TRUE -> included.
    //
    // The explicit `mask_len: Expr<Option<i32>>` binding is what actually
    // proves masklen() on a nullable field returns Expr<Option<i32>> (the
    // corrected nullable return type). A closure `-> Expr<bool>` annotation
    // alone would NOT prove it, because `eq()` returns Expr<bool> for ANY
    // T — the bound type is forced only by annotating the masklen() result
    // directly. `eq` takes `impl Into<Expr<Option<i32>>>`, which the second
    // `Expr<Option<i32>>` operand satisfies via the reflexive `Into`.
    let matched = InetAddrRow::objects()
        .filter_expr(|f| {
            let mask_len: djogi::expr::Expr<Option<i32>> = f.maybe_addr().masklen();
            mask_len.eq(f.maybe_addr().masklen())
        })
        .fetch_all(&mut ctx)
        .await
        .expect("nullable masklen() predicate executes");

    assert_eq!(
        matched.len(),
        1,
        "only the Some row matches masklen(maybe_addr) = masklen(maybe_addr); \
         the None row is excluded by three-valued logic"
    );
    assert_eq!(
        matched[0].id, some_row.id,
        "the matched row is the Some(addr) row"
    );
}
