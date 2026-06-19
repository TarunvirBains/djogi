// djogi#308 — InetAddr private field compile-fail fixture.
//
// `InetAddr` carries two private fields (`addr: std::net::IpAddr`,
// `prefix: u8`). The struct is `pub` so adopter code can name the
// type in signatures and `use` statements, but its fields are not
// exposed — construction must go through `InetAddr::new()`.
//
// This fixture attempts to construct `InetAddr` via struct literal,
// which fails because both fields are private (E0423). The `.stderr`
// snapshot pins the expected diagnostic so a future refactor that
// makes the fields public or changes the struct shape is caught.

use djogi::InetAddr;
use std::net::{IpAddr, Ipv4Addr};

fn build_via_literal() -> InetAddr {
    InetAddr {
        addr: IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5)),
        prefix: 24,
    }
}

fn main() {
    let _ = build_via_literal();
}
