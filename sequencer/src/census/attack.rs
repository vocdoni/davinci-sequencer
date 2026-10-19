//! Attack tests that need private items of `census.rs`.

use super::*;

// Ranges a metadata service or an internal host can live in, beyond the
// obvious RFC 1918 ones.
#[test]
fn is_public_special_ranges() {
    let mut leaked = Vec::new();
    [
        // IETF protocol assignments 192.0.0.0/24 (OCI metadata is 192.0.0.192)
        "192.0.0.1",
        "192.0.0.192",
        "192.0.0.255",
        // benchmarking 198.18.0.0/15
        "198.18.0.1",
        "198.19.255.255",
        "0.0.0.0",
        "127.255.255.255",
        "169.254.0.1",
        "172.16.0.0",
        "172.31.255.255",
        "255.255.255.255",
        // deprecated site-local fec0::/10
        "fec0::1",
        "feff::1",
        "ff00::1",
        "fc00::",
        "fdff:ffff::",
        // IPv4-translated ::ffff:0:a.b.c.d (RFC 2765)
        "::ffff:0:7f00:1",
        "::ffff:0:10.0.0.1",
        // IPv4-mapped in every notation
        "::ffff:7f00:1",
        "0:0:0:0:0:ffff:7f00:1",
        "::ffff:192.0.0.192",
        // NAT64 well-known prefix wrapping private v4
        "64:ff9b::a9fe:a9fe",
        "64:ff9b::192.168.1.1",
        // 6to4 and Teredo wrapping loopback
        "2002:7f00:1::",
        "2001:0:7f00:1::",
    ]
    .into_iter()
    .filter(|ip| is_public(ip.parse().unwrap()))
    .collect::<Vec<_>>()
    .iter()
    .for_each(|ip| leaked.push(*ip));
    // BUG: ranges listed here are treated as public
    assert!(leaked.is_empty(), "public but must not be: {leaked:?}");
}

// Boundaries just outside the special ranges stay reachable.
#[test]
fn is_public_neighbours() {
    for ip in [
        "172.15.255.255",
        "172.32.0.0",
        "100.63.255.255",
        "100.128.0.0",
        "169.253.0.1",
        "198.17.255.255",
        "198.20.0.0",
        "192.0.1.1",
        "223.255.255.255",
        "2606:4700::1",
        "2a00:1450::1",
    ] {
        assert!(is_public(ip.parse().unwrap()), "{ip} must be public");
    }
}
