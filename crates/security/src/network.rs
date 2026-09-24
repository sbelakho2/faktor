//! THE single egress address-classification authority.
//!
//! One definition of [`AddressClass`], [`EgressAddressPolicy`],
//! [`classify_ip`] and [`vet_resolved_answers`] lives here; every egress
//! resolver/broker (provider DNS pinning, browser broker connects, CLI
//! config validation) consumes it and must never re-derive classification
//! locally. The duplication this module removes was the defect: two
//! hand-maintained IPv4/IPv6 tables drifted, both missed IANA special
//! ranges that were added later (`64:ff9b:1::/48`, `100:0:0:1::/64`,
//! `3fff::/20`, `5f00::/16`, …), and the browser leg skipped classification
//! for literal-IP destinations entirely.
//!
//! # Data provenance (runtime never fetches)
//!
//! The classification table below is generated from pinned snapshots of
//! the IANA registries that live in `crates/security/data/`:
//!
//! * `crates/security/data/iana-ipv4-special.csv` — IANA IPv4
//!   Special-Purpose Address Registry.
//! * `crates/security/data/iana-ipv6-special.csv` — IANA IPv6
//!   Special-Purpose Address Registry.
//!
//! `scripts/generate-iana-network-table` rewrites the marker-delimited
//! `IANA_SPECIAL_TABLE` section (longest-prefix-match table) and its
//! `--check` mode fails when the committed table is stale against the CSVs.
//! `scripts/update-iana-special-registry.sh` is the ONLY network-touching
//! step and runs as an explicit operator action.
//!
//! # Policy
//!
//! | class | [`EgressAddressPolicy::EXTERNAL`] | [`EgressAddressPolicy::LOCAL`] |
//! |---|---|---|
//! | `Global` | allow | allow |
//! | `Loopback` | deny | allow (explicit rule) |
//! | everything else | deny | deny |
//!
//! Only rows whose IANA **Globally Reachable** column says `True` become
//! `Global`; every other row is a special class and is denied even by the
//! local policy (cloud metadata, RFC1918, CGNAT, documentation, ORCHID,
//! benchmarking, multicast, unspecified, reserved, …).
//!
//! # Transition/tunneling forms: documented safest handling
//!
//! `::ffff:0:0/96` (IPv4-mapped), `64:ff9b::/96` and `64:ff9b:1::/48`
//! (NAT64 translation), `2002::/16` (6to4) and `2001::/32` (Teredo) are
//! `Reserved` and are **never** classified by their embedded IPv4 address.
//! The historical classifier did exactly that, so `::ffff:8.8.8.8` read as
//! global and `::ffff:127.0.0.1` as loopback — one address family could
//! impersonate the other. In particular the NAT64 WKP row claims
//! `Globally Reachable = True` in the registry, but the prefix embeds an
//! arbitrary IPv4 destination, so it is refused rather than allowed.
//!
//! Fallthrough when no table row matches: IPv4 multicast is `Multicast`
//! (its registry is separate from the special-purpose one) and every other
//! IPv4 address is `Global`; IPv6 multicast is `Multicast`, `2000::/3` is
//! `Global` (the only allocated global-unicast range), and every other
//! IPv6 address is `Reserved`.
//!
//! # Empty/bound policy stays with the caller
//!
//! [`vet_resolved_answers`] classifies EVERY address and refuses on the
//! first refused class; the caller still owns the local rules for empty
//! answer sets and answer-count bounds (see the provider resolver and the
//! browser broker), so those operational policies cannot be silently
//! weakened by a table change.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use serde::{Deserialize, Serialize};

/// The address class of one IP, as derived from the pinned IANA
/// special-purpose registries. `Global` is the only class any policy
/// permits besides the explicit loopback rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AddressClass {
    /// Globally routable unicast (`Globally Reachable = True` in IANA, or
    /// the all-IPv4/`2000::/3` fallthrough).
    Global,
    /// `127.0.0.0/8`, `::1`.
    Loopback,
    /// RFC1918 (`10/8`, `172.16/12`, `192.168/16`) and IPv6 ULA (`fc00::/7`).
    Private,
    /// `169.254.0.0/16` (includes the cloud-metadata endpoint
    /// `169.254.169.254`) and `fe80::/10`.
    LinkLocal,
    /// `100.64.0.0/10` (carrier-grade NAT / shared address space).
    Cgnat,
    /// `192.0.2.0/24`, `198.51.100.0/24`, `203.0.113.0/24`, `2001:db8::/32`,
    /// `3fff::/20`.
    Documentation,
    /// `198.18.0.0/15`, `2001:2::/48`.
    Benchmark,
    /// `224.0.0.0/4`, `ff00::/8`.
    Multicast,
    /// `0.0.0.0/32` ("this host on this network"), `::`.
    Unspecified,
    /// Everything else outside the global unicast space: `0.0.0.0/8`,
    /// `192.0.0.0/24` (except its globally reachable anycast /32s),
    /// `192.88.99.0/24`, `240.0.0.0/4`, `100::/64`, `100:0:0:1::/64`,
    /// ORCHID/ORCHIDv2, SRv6 SIDs, and every transition/tunneling form
    /// (`::ffff:0:0/96`, `64:ff9b::/96`, `64:ff9b:1::/48`, `2002::/16`,
    /// `2001::/32`).
    Reserved,
}

impl AddressClass {
    /// Stable machine-readable label (also the `Display` form).
    pub fn as_str(self) -> &'static str {
        match self {
            AddressClass::Global => "global",
            AddressClass::Loopback => "loopback",
            AddressClass::Private => "private",
            AddressClass::LinkLocal => "link_local",
            AddressClass::Cgnat => "cgnat",
            AddressClass::Documentation => "documentation",
            AddressClass::Benchmark => "benchmark",
            AddressClass::Multicast => "multicast",
            AddressClass::Unspecified => "unspecified",
            AddressClass::Reserved => "reserved",
        }
    }
}

impl fmt::Display for AddressClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The address-class rule installed on one checked egress leg.
///
/// `EXTERNAL` (the production default) permits only [`AddressClass::Global`].
/// `LOCAL` carries the one EXPLICIT extra rule a local provider/proxy config
/// may opt into: loopback. No other special class is ever permitted — a
/// local policy still refuses link-local (cloud metadata), private, CGNAT,
/// documentation, multicast, unspecified and reserved ranges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EgressAddressPolicy {
    allow_loopback: bool,
}

impl EgressAddressPolicy {
    /// External-only: globally routable addresses only.
    pub const EXTERNAL: EgressAddressPolicy = EgressAddressPolicy {
        allow_loopback: false,
    };

    /// Local: globally routable addresses plus an explicit loopback rule.
    pub const LOCAL: EgressAddressPolicy = EgressAddressPolicy {
        allow_loopback: true,
    };

    pub const fn external() -> EgressAddressPolicy {
        Self::EXTERNAL
    }

    pub const fn local() -> EgressAddressPolicy {
        Self::LOCAL
    }

    /// The explicit rule from a destination-level `allow_loopback` flag.
    pub const fn from_allow_loopback(allow_loopback: bool) -> EgressAddressPolicy {
        EgressAddressPolicy { allow_loopback }
    }

    /// The explicit loopback rule (the only configurable class exception).
    pub const fn allow_loopback(&self) -> bool {
        self.allow_loopback
    }

    /// May an address of this class be connected to?
    pub const fn permits_class(&self, class: AddressClass) -> bool {
        match class {
            AddressClass::Global => true,
            AddressClass::Loopback => self.allow_loopback,
            AddressClass::Private
            | AddressClass::LinkLocal
            | AddressClass::Cgnat
            | AddressClass::Documentation
            | AddressClass::Benchmark
            | AddressClass::Multicast
            | AddressClass::Unspecified
            | AddressClass::Reserved => false,
        }
    }

    /// Convenience: classify then decide.
    pub fn permits(&self, ip: IpAddr) -> bool {
        self.permits_class(classify_ip(ip))
    }
}

impl Default for EgressAddressPolicy {
    fn default() -> Self {
        Self::EXTERNAL
    }
}

/// Why one address was refused by [`vet_resolved_answers`]. Carries the
/// resolved address and its class (never resolver-supplied text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressRefusal {
    /// The refused resolved address (port included).
    pub address: SocketAddr,
    /// Its classification under the pinned IANA table.
    pub class: AddressClass,
}

impl fmt::Display for AddressRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "egress address {} is in the {} address class, which the egress \
             address policy refuses",
            self.address, self.class
        )
    }
}

impl std::error::Error for AddressRefusal {}

/// One pinned IANA special-purpose row as a longest-prefix-match entry.
/// IPv4 rows keep their 4 bytes in `prefix[..4]`; the remaining bytes are 0.
pub(crate) struct IanaRow {
    v6: bool,
    len: u8,
    prefix: [u8; 16],
    class: AddressClass,
}

/// Classify one IP address against the pinned IANA longest-prefix-match
/// table. The table is generated from the checked-in CSVs by
/// `scripts/generate-iana-network-table`; the runtime never fetches or
/// parses CSV data.
pub fn classify_ip(ip: IpAddr) -> AddressClass {
    let (octets, is_v6) = match ip {
        IpAddr::V4(v4) => (v4_octets(v4), false),
        IpAddr::V6(v6) => (v6.octets(), true),
    };
    let mut class = default_class(ip);
    let mut best_len = 0u8;
    for row in IANA_SPECIAL_TABLE {
        if row.v6 != is_v6 || row.len <= best_len {
            continue;
        }
        if row_matches(row, &octets) {
            best_len = row.len;
            class = row.class;
        }
    }
    class
}

/// Vet a resolved answer set: classify EVERY address and refuse the whole
/// call when any address sits in a class the policy does not permit. The
/// set is never filtered to a permitted subset (a mixed public/private
/// answer set is a DNS-rebinding signal).
///
/// An empty slice is `Ok(())`: the calling resolver/broker owns its
/// empty-answer and answer-count bounds (see the module docs).
pub fn vet_resolved_answers(
    answers: &[SocketAddr],
    policy: EgressAddressPolicy,
) -> Result<(), AddressRefusal> {
    for address in answers {
        let class = classify_ip(address.ip());
        if !policy.permits_class(class) {
            return Err(AddressRefusal {
                address: *address,
                class,
            });
        }
    }
    Ok(())
}

fn v4_octets(v4: Ipv4Addr) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[..4].copy_from_slice(&v4.octets());
    out
}

fn row_matches(row: &IanaRow, octets: &[u8; 16]) -> bool {
    let full = (row.len / 8) as usize;
    let rem = row.len % 8;
    if row.prefix[..full] != octets[..full] {
        return false;
    }
    if rem == 0 {
        return true;
    }
    let mask = 0xffu8 << (8 - rem);
    (row.prefix[full] & mask) == (octets[full] & mask)
}

/// The class of an address no pinned row matches: multicast lives in its
/// own registry (not the special-purpose one), all other IPv4 space is
/// global unicast, and for IPv6 only `2000::/3` is allocated global
/// unicast — everything else is reserved.
fn default_class(ip: IpAddr) -> AddressClass {
    match ip {
        IpAddr::V4(v4) => {
            if v4.is_multicast() {
                AddressClass::Multicast
            } else {
                AddressClass::Global
            }
        }
        IpAddr::V6(v6) => {
            if v6.is_multicast() {
                AddressClass::Multicast
            } else if (v6.segments()[0] & 0xe000) == 0x2000 {
                AddressClass::Global
            } else {
                AddressClass::Reserved
            }
        }
    }
}

// ---------------------------------------------------------------------------
// GENERATED IANA TABLE — do not edit by hand.
// Source of truth: crates/security/data/iana-ipv4-special.csv and
// iana-ipv6-special.csv (pinned IANA special-purpose address registries).
// Regenerate: ./scripts/generate-iana-network-table   (verify: --check)
// Rows are emitted longest-prefix-first per family; classify_ip keeps the
// LONGEST match, so overlap resolution never depends on emission order.
// ---------------------------------------------------------------------------
// BEGIN GENERATED IANA TABLE
#[rustfmt::skip]
pub(crate) static IANA_SPECIAL_TABLE: &[IanaRow] = &[
    IanaRow { v6: false, len: 32, prefix: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Unspecified },
    IanaRow { v6: false, len: 32, prefix: [192, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Reserved },
    IanaRow { v6: false, len: 32, prefix: [192, 0, 0, 9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Global },
    IanaRow { v6: false, len: 32, prefix: [192, 0, 0, 10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Global },
    IanaRow { v6: false, len: 32, prefix: [192, 0, 0, 170, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Reserved },
    IanaRow { v6: false, len: 32, prefix: [192, 0, 0, 171, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Reserved },
    IanaRow { v6: false, len: 32, prefix: [192, 88, 99, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Reserved },
    IanaRow { v6: false, len: 32, prefix: [255, 255, 255, 255, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Reserved },
    IanaRow { v6: false, len: 29, prefix: [192, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Reserved },
    IanaRow { v6: false, len: 24, prefix: [192, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Reserved },
    IanaRow { v6: false, len: 24, prefix: [192, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Documentation },
    IanaRow { v6: false, len: 24, prefix: [192, 31, 196, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Global },
    IanaRow { v6: false, len: 24, prefix: [192, 52, 193, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Global },
    IanaRow { v6: false, len: 24, prefix: [192, 88, 99, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Reserved },
    IanaRow { v6: false, len: 24, prefix: [192, 175, 48, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Global },
    IanaRow { v6: false, len: 24, prefix: [198, 51, 100, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Documentation },
    IanaRow { v6: false, len: 24, prefix: [203, 0, 113, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Documentation },
    IanaRow { v6: false, len: 16, prefix: [169, 254, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::LinkLocal },
    IanaRow { v6: false, len: 16, prefix: [192, 168, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Private },
    IanaRow { v6: false, len: 15, prefix: [198, 18, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Benchmark },
    IanaRow { v6: false, len: 12, prefix: [172, 16, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Private },
    IanaRow { v6: false, len: 10, prefix: [100, 64, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Cgnat },
    IanaRow { v6: false, len: 8, prefix: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Reserved },
    IanaRow { v6: false, len: 8, prefix: [10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Private },
    IanaRow { v6: false, len: 8, prefix: [127, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Loopback },
    IanaRow { v6: false, len: 4, prefix: [240, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Reserved },
    IanaRow { v6: true, len: 128, prefix: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Unspecified },
    IanaRow { v6: true, len: 128, prefix: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1], class: AddressClass::Loopback },
    IanaRow { v6: true, len: 128, prefix: [32, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1], class: AddressClass::Global },
    IanaRow { v6: true, len: 128, prefix: [32, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2], class: AddressClass::Global },
    IanaRow { v6: true, len: 128, prefix: [32, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3], class: AddressClass::Global },
    IanaRow { v6: true, len: 96, prefix: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 255, 255, 0, 0, 0, 0], class: AddressClass::Reserved },
    IanaRow { v6: true, len: 96, prefix: [0, 100, 255, 155, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Reserved },
    IanaRow { v6: true, len: 64, prefix: [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Reserved },
    IanaRow { v6: true, len: 64, prefix: [1, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Reserved },
    IanaRow { v6: true, len: 48, prefix: [0, 100, 255, 155, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Reserved },
    IanaRow { v6: true, len: 48, prefix: [32, 1, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Benchmark },
    IanaRow { v6: true, len: 48, prefix: [32, 1, 0, 4, 1, 18, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Global },
    IanaRow { v6: true, len: 48, prefix: [38, 32, 0, 79, 128, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Global },
    IanaRow { v6: true, len: 32, prefix: [32, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Reserved },
    IanaRow { v6: true, len: 32, prefix: [32, 1, 0, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Global },
    IanaRow { v6: true, len: 32, prefix: [32, 1, 13, 184, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Documentation },
    IanaRow { v6: true, len: 28, prefix: [32, 1, 0, 16, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Reserved },
    IanaRow { v6: true, len: 28, prefix: [32, 1, 0, 32, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Global },
    IanaRow { v6: true, len: 28, prefix: [32, 1, 0, 48, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Global },
    IanaRow { v6: true, len: 23, prefix: [32, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Reserved },
    IanaRow { v6: true, len: 20, prefix: [63, 255, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Documentation },
    IanaRow { v6: true, len: 16, prefix: [32, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Reserved },
    IanaRow { v6: true, len: 16, prefix: [95, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Reserved },
    IanaRow { v6: true, len: 10, prefix: [254, 128, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::LinkLocal },
    IanaRow { v6: true, len: 7, prefix: [252, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], class: AddressClass::Private },
];
// END GENERATED IANA TABLE

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().expect("test IP")
    }

    fn sa(text: &str, port: u16) -> SocketAddr {
        SocketAddr::new(ip(text), port)
    }

    // ------------------------------------------------- classification

    #[test]
    fn external_policy_refuses_every_local_literal() {
        for text in [
            "127.0.0.1",
            "169.254.169.254",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "100.64.0.1",
            "::1",
            "fe80::1",
            "fc00::1",
        ] {
            let address = sa(text, 443);
            assert!(
                vet_resolved_answers(&[address], EgressAddressPolicy::EXTERNAL).is_err(),
                "{text} must be refused under EXTERNAL"
            );
            assert!(
                !EgressAddressPolicy::EXTERNAL.permits(address.ip()),
                "{text} must not be permitted under EXTERNAL"
            );
        }
    }

    #[test]
    fn local_policy_permits_only_the_loopback_class() {
        for text in ["127.0.0.1", "::1"] {
            assert!(
                vet_resolved_answers(&[sa(text, 80)], EgressAddressPolicy::LOCAL).is_ok(),
                "{text} is the explicit local rule"
            );
            assert!(EgressAddressPolicy::LOCAL.permits(ip(text)));
        }
        for text in [
            "169.254.169.254",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "100.64.0.1",
            "fe80::1",
            "fc00::1",
            "224.0.0.1",
            "ff02::1",
            "0.0.0.0",
            "240.0.0.1",
            "2001:db8::1",
        ] {
            assert!(
                vet_resolved_answers(&[sa(text, 80)], EgressAddressPolicy::LOCAL).is_err(),
                "LOCAL must still refuse {text}"
            );
        }
    }

    #[test]
    fn mixed_answer_sets_are_refused_whole_and_the_refusal_names_the_class() {
        let answers = [sa("93.184.216.34", 443), sa("10.0.0.1", 443)];
        let refusal = vet_resolved_answers(&answers, EgressAddressPolicy::EXTERNAL)
            .expect_err("private answer in a mixed set");
        assert_eq!(refusal.class, AddressClass::Private);
        assert_eq!(refusal.address, sa("10.0.0.1", 443));
        assert!(format!("{refusal}").contains("private"), "{refusal}");
        // Public-first ordering must not "filter to the allowed subset".
        let answers = [sa("93.184.216.34", 443), sa("::ffff:127.0.0.1", 443)];
        assert!(vet_resolved_answers(&answers, EgressAddressPolicy::LOCAL).is_err());
    }

    #[test]
    fn tunneling_and_embedded_forms_are_denied_not_classified_by_embedded_ipv4() {
        // The historical bypass: ::ffff:8.8.8.8 read as global,
        // ::ffff:127.0.0.1 as loopback, 64:ff9b::a00:1 as private.
        for text in [
            "::ffff:127.0.0.1",
            "::ffff:8.8.8.8",
            "::ffff:169.254.169.254",
            "64:ff9b::7f00:1",
            "64:ff9b::808:808",
            "64:ff9b::a00:1",
            "2002:7f00:1::",
            "2002:808:808::",
            "2001::1",
        ] {
            assert_ne!(
                classify_ip(ip(text)),
                AddressClass::Global,
                "{text} must never be Global"
            );
            assert!(
                vet_resolved_answers(&[sa(text, 443)], EgressAddressPolicy::LOCAL).is_err(),
                "{text} must be refused even by LOCAL"
            );
        }
        assert_eq!(classify_ip(ip("::ffff:0:0")), AddressClass::Reserved);
        assert_eq!(classify_ip(ip("64:ff9b::")), AddressClass::Reserved);
        assert_eq!(classify_ip(ip("2002::")), AddressClass::Reserved);
    }

    #[test]
    fn previously_missed_iana_ranges_carry_their_special_classes() {
        // Ranges the deleted hand-maintained tables missed entirely.
        assert_eq!(classify_ip(ip("64:ff9b:1::1")), AddressClass::Reserved);
        assert_eq!(classify_ip(ip("100:0:0:1::1")), AddressClass::Reserved);
        assert_eq!(classify_ip(ip("3fff::1")), AddressClass::Documentation);
        assert_eq!(classify_ip(ip("5f00::1")), AddressClass::Reserved);
        // Longest-prefix resolution: the globally reachable anycast /32s
        // inside a non-global /24 stay Global; their neighbours do not.
        assert_eq!(classify_ip(ip("192.0.0.9")), AddressClass::Global);
        assert_eq!(classify_ip(ip("192.0.0.10")), AddressClass::Global);
        assert_eq!(classify_ip(ip("192.0.0.8")), AddressClass::Reserved);
        assert_eq!(classify_ip(ip("192.0.0.1")), AddressClass::Reserved);
        assert_eq!(classify_ip(ip("2001:1::3")), AddressClass::Global);
        assert_eq!(classify_ip(ip("2001:2::1")), AddressClass::Benchmark);
        assert_eq!(classify_ip(ip("2001:db8::1")), AddressClass::Documentation);
    }

    #[test]
    fn globals_and_defaults_cover_the_unregistered_space() {
        for text in [
            "8.8.8.8",
            "1.1.1.1",
            "93.184.216.34",
            "192.31.196.1",
            "192.52.193.1",
            "192.175.48.1",
            "2606:4700::1111",
            "2001:4860:4860::8888",
            "2001:4:112::1",
        ] {
            assert_eq!(classify_ip(ip(text)), AddressClass::Global, "{text}");
        }
        assert_eq!(classify_ip(ip("127.0.0.1")), AddressClass::Loopback);
        assert_eq!(classify_ip(ip("::1")), AddressClass::Loopback);
        assert_eq!(classify_ip(ip("0.0.0.0")), AddressClass::Unspecified);
        assert_eq!(classify_ip(ip("::")), AddressClass::Unspecified);
        assert_eq!(classify_ip(ip("169.254.169.254")), AddressClass::LinkLocal);
        assert_eq!(classify_ip(ip("10.0.0.1")), AddressClass::Private);
        assert_eq!(classify_ip(ip("172.16.0.1")), AddressClass::Private);
        assert_eq!(classify_ip(ip("172.31.255.254")), AddressClass::Private);
        assert_eq!(classify_ip(ip("192.168.1.1")), AddressClass::Private);
        assert_eq!(classify_ip(ip("100.64.0.1")), AddressClass::Cgnat);
        assert_eq!(classify_ip(ip("100.127.255.254")), AddressClass::Cgnat);
        assert_eq!(classify_ip(ip("fe80::1")), AddressClass::LinkLocal);
        assert_eq!(classify_ip(ip("fc00::1")), AddressClass::Private);
        assert_eq!(classify_ip(ip("fd12:3456::1")), AddressClass::Private);
        // Multicast registries are separate from the special-purpose ones:
        // the fallthrough must still deny it.
        assert_eq!(classify_ip(ip("224.0.0.1")), AddressClass::Multicast);
        assert_eq!(classify_ip(ip("239.255.255.255")), AddressClass::Multicast);
        assert_eq!(classify_ip(ip("ff02::1")), AddressClass::Multicast);
        // Unallocated IPv6 space outside 2000::/3 is reserved, not global.
        assert_eq!(classify_ip(ip("4000::1")), AddressClass::Reserved);
        assert_eq!(classify_ip(ip("::2")), AddressClass::Reserved);
        assert_eq!(classify_ip(ip("240.0.0.1")), AddressClass::Reserved);
        assert_eq!(classify_ip(ip("255.255.255.255")), AddressClass::Reserved);
        assert_eq!(classify_ip(ip("0.1.2.3")), AddressClass::Reserved);
        assert_eq!(classify_ip(ip("192.88.99.1")), AddressClass::Reserved);
    }

    #[test]
    fn empty_answer_sets_are_the_callers_operational_rule() {
        assert!(vet_resolved_answers(&[], EgressAddressPolicy::EXTERNAL).is_ok());
    }

    // ---------------------------------------------- pinned-CSV drift guard

    /// Minimal RFC4180 CSV parse (quotes, doubled quotes, embedded
    /// newlines) for the pinned registry files — test-only, so no runtime
    /// dependency is added.
    fn parse_csv(src: &str) -> Vec<Vec<String>> {
        let mut rows = Vec::new();
        let mut row: Vec<String> = Vec::new();
        let mut field = String::new();
        let mut chars = src.chars().peekable();
        let mut in_quotes = false;
        while let Some(c) = chars.next() {
            if in_quotes {
                if c == '"' {
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        field.push('"');
                    } else {
                        in_quotes = false;
                    }
                } else {
                    field.push(c);
                }
            } else {
                match c {
                    '"' => in_quotes = true,
                    ',' => row.push(std::mem::take(&mut field)),
                    '\r' => {}
                    '\n' => {
                        row.push(std::mem::take(&mut field));
                        rows.push(std::mem::take(&mut row));
                    }
                    _ => field.push(c),
                }
            }
        }
        if !field.is_empty() || !row.is_empty() {
            row.push(field);
            rows.push(row);
        }
        rows
    }

    /// `(address block, name, globally reachable)` for every pinned row,
    /// one tuple per address block (a row may carry several).
    fn registry_rows(src: &str) -> Vec<(String, String, bool)> {
        let rows = parse_csv(src);
        let header = &rows[0];
        let column = |name: &str| {
            header
                .iter()
                .position(|h| h.trim() == name)
                .unwrap_or_else(|| panic!("missing column {name}"))
        };
        let (addr_col, name_col, reach_col) = (
            column("Address Block"),
            column("Name"),
            column("Globally Reachable"),
        );
        let strip = |text: &str| {
            let mut out = String::new();
            let mut depth = 0usize;
            for c in text.chars() {
                match c {
                    '[' => depth += 1,
                    ']' => depth = depth.saturating_sub(1),
                    _ if depth == 0 => out.push(c),
                    _ => {}
                }
            }
            out.trim().to_string()
        };
        let mut out = Vec::new();
        for row in rows.iter().skip(1) {
            if row.iter().all(|cell| cell.trim().is_empty()) {
                continue;
            }
            let name = strip(row.get(name_col).map(String::as_str).unwrap_or(""));
            let reachable = strip(row.get(reach_col).map(String::as_str).unwrap_or(""))
                .eq_ignore_ascii_case("true");
            for block in row
                .get(addr_col)
                .map(String::as_str)
                .unwrap_or("")
                .split(',')
            {
                let block = strip(block);
                if !block.is_empty() {
                    out.push((block, name.clone(), reachable));
                }
            }
        }
        out
    }

    fn is_tunneling(name: &str) -> bool {
        let folded = name.to_ascii_lowercase();
        ["ipv4-mapped", "ipv4-ipv6 translat", "6to4", "teredo"]
            .iter()
            .any(|needle| folded.contains(needle))
    }

    /// The network address and the last address of one `addr/len` block: the
    /// two extremes pin the generated prefix and length.
    fn block_extremes(block: &str) -> (IpAddr, IpAddr) {
        use std::net::{Ipv4Addr, Ipv6Addr};
        let (addr_text, len_text) = block
            .split_once('/')
            .unwrap_or_else(|| panic!("block without prefix length: {block:?}"));
        let len: u32 = len_text
            .parse()
            .unwrap_or_else(|_| panic!("bad prefix length in {block:?}"));
        match addr_text
            .parse::<IpAddr>()
            .unwrap_or_else(|_| panic!("unparseable block {block:?}"))
        {
            IpAddr::V4(v4) => {
                let host_bits = if len == 0 {
                    u32::MAX
                } else {
                    (1u64 << (32 - len)).saturating_sub(1) as u32
                };
                (
                    IpAddr::V4(v4),
                    IpAddr::V4(Ipv4Addr::from(u32::from(v4) | host_bits)),
                )
            }
            IpAddr::V6(v6) => {
                let host_bits = if len == 0 {
                    u128::MAX
                } else {
                    (1u128 << (128 - len)) - 1
                };
                (
                    IpAddr::V6(v6),
                    IpAddr::V6(Ipv6Addr::from(u128::from(v6) | host_bits)),
                )
            }
        }
    }

    #[test]
    fn committed_table_tracks_the_pinned_registry_csvs() {
        // The committed static table must agree with the pinned CSVs row by
        // row: every row explicitly marked Globally Reachable=True becomes
        // Global (except the documented tunneling override), and every other
        // row is never Global. Both the first and the last address of each
        // block are probed, so a wrong prefix/length cannot pass. A
        // dropped/added/misclassified generated row fails here even if nobody
        // ran the generator's --check.
        for src in [
            include_str!("../data/iana-ipv4-special.csv"),
            include_str!("../data/iana-ipv6-special.csv"),
        ] {
            let rows = registry_rows(src);
            assert!(rows.len() >= 20, "suspiciously small registry: {rows:?}");
            for (block, name, reachable) in rows {
                let (first, last) = block_extremes(&block);
                for probe in [first, last] {
                    let class = classify_ip(probe);
                    if reachable && !is_tunneling(&name) {
                        assert_eq!(
                            class,
                            AddressClass::Global,
                            "{block} ({name}) is Globally Reachable but {probe} is {class:?}"
                        );
                    } else {
                        assert_ne!(
                            class,
                            AddressClass::Global,
                            "{block} ({name}) must never become Global but {probe} is"
                        );
                    }
                }
            }
        }
    }
}
