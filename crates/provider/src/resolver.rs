//! Central egress resolver: one DNS resolution, every answer classified,
//! a pin to a vetted [`SocketAddr`] set — closing the DNS-rebinding/SSRF
//! class where a hostname is checked by name and then connected by name.
//!
//! The defect this module closes: the parsed-destination gate compares the
//! URL host TEXT, but the TCP connect later resolves that name again and
//! blindly connects to whatever it answers. An allowlisted hostname that
//! resolves to `127.0.0.1`, `169.254.169.254`, an RFC1918 address, `::1`,
//! `fe80::1`, a CGNAT or a documentation range therefore reached process-
//! local, LAN or cloud-metadata endpoints.
//!
//! [`EgressResolver`] resolves a permitted hostname **exactly once**
//! through the injected [`HostResolver`], bounds the answer set
//! ([`EgressResolver::max_answers`]), classifies EVERY address
//! ([`AddressClass`]) and refuses the whole resolution when any answer sits
//! in a class the [`EgressAddressPolicy`] does not permit (an "external"
//! policy permits only globally routable addresses; a mixed public/private
//! answer set is refused, never raced). The caller then connects to the
//! returned addresses directly — there is no second resolution between the
//! decision and the connect.
//!
//! The production reqwest client installs [`ReqwestDnsResolver`] as its
//! `dns_resolver`, so hyper's connector receives the vetted address list
//! and tries exactly those addresses. IP-literal URLs never reach a
//! resolver at all, which is why the request-time gate also classifies a
//! literal host directly (see `egress::check_url`).
//!
//! # Policy classes
//!
//! | class                              | external policy | [`EgressAddressPolicy::local`] |
//! |------------------------------------|-----------------|-------------------------------|
//! | `Global`                           | allow           | allow                         |
//! | `Loopback`                         | deny            | allow (explicit rule)         |
//! | private / link-local / CGNAT /     | deny            | deny                          |
//! | documentation / benchmark /        |                 |                               |
//! | multicast / unspecified / reserved |                 |                               |
//!
//! A local provider (Ollama) gets the explicit [`EgressAddressPolicy::local`]
//! rule wired from its typed config — never a global "loopback is always
//! allowed" exception. Everything except loopback stays refused even there,
//! so a configured local endpoint can never be rebound onto cloud metadata,
//! a LAN host or any other special range.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};

/// Default hard bound on the number of addresses one resolution may answer
/// with. A hostile or broken DNS server answering with an unbounded list is
/// refused (never truncated into a racing subset).
pub const DEFAULT_MAX_DNS_ANSWERS: usize = 16;

/// Absolute DNS host length (RFC 1035 presentation form).
pub const MAX_DNS_HOST_CHARS: usize = 253;

/// The address class of one resolved IP, as classified by the central
/// egress resolver. The tables mirror the IANA special-purpose registries;
/// `Global` is the only class an external policy allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AddressClass {
    /// Globally routable unicast.
    Global,
    /// `127.0.0.0/8`, `::1`.
    Loopback,
    /// RFC1918 (`10/8`, `172.16/12`, `192.168/16`) and IPv6 ULA (`fc00::/7`).
    Private,
    /// `169.254.0.0/16` (includes the cloud-metadata endpoint
    /// `169.254.169.254`) and `fe80::/10`.
    LinkLocal,
    /// `100.64.0.0/10` (carrier-grade NAT).
    Cgnat,
    /// `192.0.2.0/24`, `198.51.100.0/24`, `203.0.113.0/24`, `2001:db8::/32`.
    Documentation,
    /// `198.18.0.0/15`, `2001:2::/48`.
    Benchmark,
    /// `224.0.0.0/4`, `ff00::/8`.
    Multicast,
    /// `0.0.0.0`, `::`.
    Unspecified,
    /// Everything else outside the global unicast space: `0.0.0.0/8`,
    /// `192.0.0.0/24`, `192.88.99.0/24`, `240.0.0.0/4`, `100::/64`,
    /// `2001:10::/28`, `2001:20::/28`.
    Reserved,
}

impl AddressClass {
    /// Classify one IP. IPv4-mapped (`::ffff:a.b.c.d`), IPv4-compatible
    /// (`::a.b.c.d`), 6to4 (`2002::/16`) and NAT64 (`64:ff9b::/96`)
    /// addresses classify by the EMBEDDED IPv4 address — otherwise
    /// `::ffff:127.0.0.1` would read as a global IPv6 address.
    pub fn classify(ip: IpAddr) -> AddressClass {
        match ip {
            IpAddr::V4(v4) => classify_v4(v4),
            IpAddr::V6(v6) => classify_v6(v6),
        }
    }

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

impl std::fmt::Display for AddressClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

fn classify_v4(v4: Ipv4Addr) -> AddressClass {
    let o = v4.octets();
    if v4.is_unspecified() {
        return AddressClass::Unspecified;
    }
    if v4.is_loopback() {
        return AddressClass::Loopback;
    }
    if v4.is_link_local() {
        return AddressClass::LinkLocal;
    }
    if v4.is_private() {
        return AddressClass::Private;
    }
    if v4.is_multicast() {
        return AddressClass::Multicast;
    }
    if o[0] == 100 && (64..=127).contains(&o[1]) {
        return AddressClass::Cgnat;
    }
    if v4.is_documentation() {
        return AddressClass::Documentation;
    }
    // 198.18.0.0/15 (benchmarking).
    if o[0] == 198 && (o[1] == 18 || o[1] == 19) {
        return AddressClass::Benchmark;
    }
    // 0.0.0.0/8 "this network" (0.0.0.0 itself is Unspecified above),
    // 192.0.0.0/24 IETF protocol assignments, 192.88.99.0/24 (6to4
    // relay, deprecated), 240.0.0.0/4 reserved.
    if o[0] == 0
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        || (o[0] == 192 && o[1] == 88 && o[2] == 99)
        || o[0] >= 240
    {
        return AddressClass::Reserved;
    }
    AddressClass::Global
}

fn classify_v6(v6: Ipv6Addr) -> AddressClass {
    let s = v6.segments();
    if v6.is_unspecified() {
        return AddressClass::Unspecified;
    }
    if v6.is_loopback() {
        return AddressClass::Loopback;
    }
    if v6.is_multicast() {
        return AddressClass::Multicast;
    }
    // fe80::/10 link-local (includes fe80::/64 interface scope).
    if (s[0] & 0xffc0) == 0xfe80 {
        return AddressClass::LinkLocal;
    }
    // fc00::/7 unique-local.
    if (s[0] & 0xfe00) == 0xfc00 {
        return AddressClass::Private;
    }
    // IPv4-mapped ::ffff:0:0/96.
    if s[0] == 0 && s[1] == 0 && s[2] == 0 && s[3] == 0 && s[4] == 0 && s[5] == 0xffff {
        return classify_v4(embedded_v4(s[6], s[7]));
    }
    // IPv4-compatible ::a.b.c.d (/96, deprecated); :: and ::1 handled above.
    if s[0] == 0 && s[1] == 0 && s[2] == 0 && s[3] == 0 && s[4] == 0 && s[5] == 0 {
        return classify_v4(embedded_v4(s[6], s[7]));
    }
    // 6to4 2002::/16 embeds the IPv4 address in segments 1-2.
    if s[0] == 0x2002 {
        return classify_v4(embedded_v4(s[1], s[2]));
    }
    // NAT64 64:ff9b::/96 embeds the IPv4 address in the low 32 bits.
    if s[0] == 0x0064 && s[1] == 0xff9b && s[2] == 0 && s[3] == 0 && s[4] == 0 && s[5] == 0 {
        return classify_v4(embedded_v4(s[6], s[7]));
    }
    // 2001:db8::/32 documentation.
    if s[0] == 0x2001 && s[1] == 0x0db8 {
        return AddressClass::Documentation;
    }
    // 2001:2::/48 benchmarking.
    if s[0] == 0x2001 && s[1] == 0x0002 && s[2] == 0 {
        return AddressClass::Benchmark;
    }
    // 100::/64 discard-only, 2001:10::/28 and 2001:20::/28 ORCHID.
    if (s[0] == 0x0100 && s[1] == 0 && s[2] == 0 && s[3] == 0)
        || (s[0] == 0x2001 && (s[1] & 0xfff0) == 0x0010)
        || (s[0] == 0x2001 && (s[1] & 0xfff0) == 0x0020)
    {
        return AddressClass::Reserved;
    }
    AddressClass::Global
}

fn embedded_v4(hi: u16, lo: u16) -> Ipv4Addr {
    Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8)
}

/// The address-class rule installed on one checked egress client.
///
/// `EXTERNAL` (the production default) permits only [`AddressClass::Global`].
/// `LOCAL` carries the one EXPLICIT extra rule a local provider config may
/// opt into: loopback. No other special class is ever permitted — a local
/// policy still refuses link-local (cloud metadata), private, CGNAT,
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
        self.permits_class(AddressClass::classify(ip))
    }
}

impl Default for EgressAddressPolicy {
    fn default() -> Self {
        Self::EXTERNAL
    }
}

/// Why a resolution was refused before any connect. Carries no raw URL and
/// no resolver-supplied text (a hostile resolver message could echo the
/// queried name); the host is the parsed request host, bounded by the URL
/// layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EgressResolveError {
    EmptyHost,
    /// The host exceeds the DNS presentation bound — refused, never looked
    /// up.
    HostTooLong {
        chars: usize,
        limit: usize,
    },
    /// The underlying lookup failed (resolver error). No resolver text is
    /// carried.
    LookupFailed,
    /// The lookup returned no addresses.
    NoAddresses,
    /// More answers than [`EgressResolver::max_answers`].
    TooManyAnswers {
        count: usize,
        limit: usize,
    },
    /// At least one answer sits in a class the address policy refuses. The
    /// WHOLE resolution is refused (never filtered to the permitted
    /// subset): a mixed public/private answer set is a rebinding signal.
    AddressClassRefused {
        host: String,
        class: AddressClass,
    },
}

impl std::fmt::Display for EgressResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EgressResolveError::EmptyHost => write!(f, "egress resolve refused: empty host"),
            EgressResolveError::HostTooLong { chars, limit } => write!(
                f,
                "egress resolve refused: host of {chars} chars exceeds the {limit}-char DNS bound"
            ),
            EgressResolveError::LookupFailed => {
                write!(f, "egress resolve failed: DNS lookup error")
            }
            EgressResolveError::NoAddresses => {
                write!(f, "egress resolve failed: DNS returned no addresses")
            }
            EgressResolveError::TooManyAnswers { count, limit } => write!(
                f,
                "egress resolve refused: DNS answered with {count} addresses, over the bound \
                 of {limit}"
            ),
            EgressResolveError::AddressClassRefused { host, class } => write!(
                f,
                "egress resolve refused: host {host:?} resolved to a {class} address, which \
                 the egress address policy refuses"
            ),
        }
    }
}

impl std::error::Error for EgressResolveError {}

/// The DNS seam: one hostname → address list. Production uses
/// [`SystemHostResolver`]; tests inject scripted answers (malicious DNS is
/// exactly what must be testable).
pub trait HostResolver: Send + Sync + std::fmt::Debug {
    fn resolve(&self, host: &str, port: u16) -> BoxFuture<'_, Result<Vec<SocketAddr>, String>>;
}

/// The OS resolver (`getaddrinfo`).
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemHostResolver;

impl HostResolver for SystemHostResolver {
    fn resolve(&self, host: &str, port: u16) -> BoxFuture<'_, Result<Vec<SocketAddr>, String>> {
        let host = host.to_string();
        Box::pin(async move {
            tokio::net::lookup_host((host.as_str(), port))
                .await
                .map(|addrs| addrs.collect())
                .map_err(|e| e.to_string())
        })
    }
}

/// The central egress resolver: resolve once, bound the answers, classify
/// every address, refuse the whole set when any answer is outside the
/// permitted classes.
#[derive(Debug, Clone)]
pub struct EgressResolver {
    resolver: Arc<dyn HostResolver>,
    policy: EgressAddressPolicy,
    max_answers: usize,
}

impl EgressResolver {
    /// The production resolver over the OS resolver.
    pub fn system(policy: EgressAddressPolicy) -> EgressResolver {
        EgressResolver {
            resolver: Arc::new(SystemHostResolver),
            policy,
            max_answers: DEFAULT_MAX_DNS_ANSWERS,
        }
    }

    /// A resolver over an injected [`HostResolver`] (tests inject malicious
    /// DNS answers; the answer bound is explicit).
    pub fn with_resolver(
        resolver: Arc<dyn HostResolver>,
        policy: EgressAddressPolicy,
        max_answers: usize,
    ) -> EgressResolver {
        EgressResolver {
            resolver,
            policy,
            max_answers: max_answers.max(1),
        }
    }

    pub fn policy(&self) -> EgressAddressPolicy {
        self.policy
    }

    pub fn max_answers(&self) -> usize {
        self.max_answers
    }

    /// Resolve `host` EXACTLY ONCE and return the vetted address set. Every
    /// address must be permitted; an empty set, an over-bound set or any
    /// refused class is a typed error and nothing is connected.
    pub async fn resolve_vetted(
        &self,
        host: &str,
        port: u16,
    ) -> Result<Vec<SocketAddr>, EgressResolveError> {
        if host.is_empty() {
            return Err(EgressResolveError::EmptyHost);
        }
        if host.chars().count() > MAX_DNS_HOST_CHARS {
            return Err(EgressResolveError::HostTooLong {
                chars: host.chars().count(),
                limit: MAX_DNS_HOST_CHARS,
            });
        }
        let answers = self
            .resolver
            .resolve(host, port)
            .await
            .map_err(|_| EgressResolveError::LookupFailed)?;
        self.vet(host, answers)
    }

    /// The pure vetting half: bound + classify + decide. Split out so the
    /// classification contract is testable without any network seam.
    pub fn vet(
        &self,
        host: &str,
        answers: Vec<SocketAddr>,
    ) -> Result<Vec<SocketAddr>, EgressResolveError> {
        if answers.is_empty() {
            return Err(EgressResolveError::NoAddresses);
        }
        if answers.len() > self.max_answers {
            return Err(EgressResolveError::TooManyAnswers {
                count: answers.len(),
                limit: self.max_answers,
            });
        }
        for addr in &answers {
            let class = AddressClass::classify(addr.ip());
            if !self.policy.permits_class(class) {
                return Err(EgressResolveError::AddressClassRefused {
                    host: host.to_string(),
                    class,
                });
            }
        }
        Ok(answers)
    }
}

/// `reqwest::dns::Resolve` adapter: hyper's connector asks for a hostname,
/// receives exactly the vetted address list, and tries those addresses
/// without a second resolution.
#[derive(Debug, Clone)]
pub struct ReqwestDnsResolver {
    resolver: EgressResolver,
}

impl ReqwestDnsResolver {
    pub fn new(resolver: EgressResolver) -> Self {
        Self { resolver }
    }

    pub fn resolver(&self) -> &EgressResolver {
        &self.resolver
    }
}

impl reqwest::dns::Resolve for ReqwestDnsResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        let resolver = self.resolver.clone();
        Box::pin(async move {
            // Port 0: the URL's explicit/effective port is applied by the
            // connector (`set_port`), and the class decision is port-free.
            let addrs = resolver
                .resolve_vetted(&host, 0)
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::net::{Ipv4Addr, Ipv6Addr};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    fn v4(s: &str) -> IpAddr {
        IpAddr::V4(s.parse::<Ipv4Addr>().unwrap())
    }

    fn v6(s: &str) -> IpAddr {
        IpAddr::V6(s.parse::<Ipv6Addr>().unwrap())
    }

    fn addr(s: &str, port: u16) -> SocketAddr {
        SocketAddr::new(if s.contains(':') { v6(s) } else { v4(s) }, port)
    }

    #[test]
    fn classification_covers_the_special_ranges() {
        let cases: &[(&str, AddressClass)] = &[
            ("8.8.8.8", AddressClass::Global),
            ("1.1.1.1", AddressClass::Global),
            ("127.0.0.1", AddressClass::Loopback),
            ("127.9.9.9", AddressClass::Loopback),
            ("169.254.169.254", AddressClass::LinkLocal),
            ("169.254.0.1", AddressClass::LinkLocal),
            ("10.0.0.1", AddressClass::Private),
            ("172.16.0.1", AddressClass::Private),
            ("172.31.255.254", AddressClass::Private),
            ("192.168.1.1", AddressClass::Private),
            ("100.64.0.1", AddressClass::Cgnat),
            ("100.127.255.254", AddressClass::Cgnat),
            ("192.0.2.1", AddressClass::Documentation),
            ("198.51.100.1", AddressClass::Documentation),
            ("203.0.113.1", AddressClass::Documentation),
            ("198.18.0.1", AddressClass::Benchmark),
            ("198.19.255.254", AddressClass::Benchmark),
            ("224.0.0.1", AddressClass::Multicast),
            ("239.255.255.255", AddressClass::Multicast),
            ("0.0.0.0", AddressClass::Unspecified),
            ("0.1.2.3", AddressClass::Reserved),
            ("192.0.0.8", AddressClass::Reserved),
            ("240.0.0.1", AddressClass::Reserved),
            ("255.255.255.255", AddressClass::Reserved),
            ("::", AddressClass::Unspecified),
            ("::1", AddressClass::Loopback),
            ("2606:4700::1111", AddressClass::Global),
            ("2001:4860:4860::8888", AddressClass::Global),
            ("fe80::1", AddressClass::LinkLocal),
            ("fe80::abcd:1", AddressClass::LinkLocal),
            ("fc00::1", AddressClass::Private),
            ("fd12:3456::1", AddressClass::Private),
            ("ff02::1", AddressClass::Multicast),
            ("2001:db8::1", AddressClass::Documentation),
            ("2001:2::1", AddressClass::Benchmark),
            ("100::1", AddressClass::Reserved),
            // Embedded IPv4 must classify by the embedded address.
            ("::ffff:127.0.0.1", AddressClass::Loopback),
            ("::ffff:169.254.169.254", AddressClass::LinkLocal),
            ("::ffff:8.8.8.8", AddressClass::Global),
            ("64:ff9b::127.0.0.1", AddressClass::Loopback),
            ("64:ff9b::a00:1", AddressClass::Private),
            ("2002:7f00:1::", AddressClass::Loopback),
            ("2002:0808:0808::", AddressClass::Global),
        ];
        for (text, want) in cases {
            let ip = if text.contains(':') {
                v6(text)
            } else {
                v4(text)
            };
            assert_eq!(AddressClass::classify(ip), *want, "{text}");
        }
    }

    #[test]
    fn external_policy_allows_only_global_and_local_adds_loopback_only() {
        for ip in ["8.8.8.8", "2606:4700::1111", "::ffff:8.8.8.8"] {
            let ip = if ip.contains(':') { v6(ip) } else { v4(ip) };
            assert!(EgressAddressPolicy::EXTERNAL.permits(ip), "{ip}");
            assert!(EgressAddressPolicy::LOCAL.permits(ip), "{ip}");
        }
        for ip in [
            "127.0.0.1",
            "169.254.169.254",
            "10.0.0.1",
            "100.64.0.1",
            "192.0.2.1",
            "0.0.0.0",
            "224.0.0.1",
            "240.0.0.1",
            "::1",
            "fe80::1",
            "fc00::1",
            "2001:db8::1",
            "::ffff:127.0.0.1",
        ] {
            let ip = if ip.contains(':') { v6(ip) } else { v4(ip) };
            assert!(
                !EgressAddressPolicy::EXTERNAL.permits(ip),
                "external must refuse {ip}"
            );
        }
        // The explicit local rule permits ONLY loopback: metadata,
        // link-local, private and every other class stay refused.
        for ip in ["127.0.0.1", "::1", "::ffff:127.0.0.1"] {
            let ip = if ip.contains(':') { v6(ip) } else { v4(ip) };
            assert!(EgressAddressPolicy::LOCAL.permits(ip), "local allows {ip}");
        }
        for ip in [
            "169.254.169.254",
            "10.0.0.1",
            "100.64.0.1",
            "fe80::1",
            "fc00::1",
            "2001:db8::1",
            "0.0.0.0",
            "224.0.0.1",
        ] {
            let ip = if ip.contains(':') { v6(ip) } else { v4(ip) };
            assert!(
                !EgressAddressPolicy::LOCAL.permits(ip),
                "local must still refuse {ip}"
            );
        }
    }

    /// Counting scripted resolver: one canned answer list per call.
    #[derive(Debug, Default)]
    struct ScriptedResolver {
        answers: Mutex<VecDeque<Result<Vec<SocketAddr>, String>>>,
        calls: AtomicUsize,
        seen_hosts: Mutex<Vec<String>>,
    }

    impl ScriptedResolver {
        fn answering(answers: Vec<SocketAddr>) -> Arc<Self> {
            let r = ScriptedResolver::default();
            r.answers.lock().unwrap().push_back(Ok(answers));
            Arc::new(r)
        }

        fn failing() -> Arc<Self> {
            let r = ScriptedResolver::default();
            r.answers
                .lock()
                .unwrap()
                .push_back(Err("dns exploded".into()));
            Arc::new(r)
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn seen_hosts(&self) -> Vec<String> {
            self.seen_hosts.lock().unwrap().clone()
        }
    }

    impl HostResolver for ScriptedResolver {
        fn resolve(&self, host: &str, port: u16) -> BoxFuture<'_, Result<Vec<SocketAddr>, String>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen_hosts.lock().unwrap().push(host.to_string());
            let next = self
                .answers
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(Vec::new()));
            let _ = port;
            Box::pin(async move { next })
        }
    }

    #[tokio::test]
    async fn malicious_answers_for_an_allowlisted_hostname_are_refused() {
        for (label, answer) in [
            ("loopback", "127.0.0.1"),
            ("metadata", "169.254.169.254"),
            ("private", "10.0.0.1"),
            ("loopback v6", "::1"),
            ("link-local v6", "fe80::1"),
            ("mapped loopback", "::ffff:127.0.0.1"),
        ] {
            let resolver = ScriptedResolver::answering(vec![addr(answer, 443)]);
            let egress = EgressResolver::with_resolver(
                resolver.clone(),
                EgressAddressPolicy::EXTERNAL,
                DEFAULT_MAX_DNS_ANSWERS,
            );
            let err = egress
                .resolve_vetted("allowed.example", 443)
                .await
                .expect_err(label);
            match err {
                EgressResolveError::AddressClassRefused { class, .. } => {
                    assert_ne!(class, AddressClass::Global, "{label}");
                }
                other => panic!("{label}: expected class refusal, got {other:?}"),
            }
            assert_eq!(resolver.calls(), 1, "{label}: exactly one resolution");
        }
    }

    #[tokio::test]
    async fn explicit_allow_loopback_permits_only_the_local_case() {
        let resolver = ScriptedResolver::answering(vec![addr("127.0.0.1", 11434)]);
        let egress = EgressResolver::with_resolver(
            resolver,
            EgressAddressPolicy::LOCAL,
            DEFAULT_MAX_DNS_ANSWERS,
        );
        let vetted = egress.resolve_vetted("localhost", 11434).await.unwrap();
        assert_eq!(vetted, vec![addr("127.0.0.1", 11434)]);
        // The same explicit rule never opens link-local/metadata.
        let resolver = ScriptedResolver::answering(vec![addr("169.254.169.254", 80)]);
        let egress = EgressResolver::with_resolver(
            resolver,
            EgressAddressPolicy::LOCAL,
            DEFAULT_MAX_DNS_ANSWERS,
        );
        assert!(matches!(
            egress.resolve_vetted("metadata.example", 80).await,
            Err(EgressResolveError::AddressClassRefused {
                class: AddressClass::LinkLocal,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn mixed_public_and_private_answer_sets_are_refused_whole() {
        // Public first, private second: a filtering implementation would
        // "connect to the public one" and race the hostile answer. The
        // whole set is refused.
        let resolver =
            ScriptedResolver::answering(vec![addr("93.184.216.34", 443), addr("10.0.0.1", 443)]);
        let egress = EgressResolver::with_resolver(
            resolver,
            EgressAddressPolicy::EXTERNAL,
            DEFAULT_MAX_DNS_ANSWERS,
        );
        assert!(matches!(
            egress.resolve_vetted("mixed.example", 443).await,
            Err(EgressResolveError::AddressClassRefused {
                class: AddressClass::Private,
                ..
            })
        ));
        // Private first, public second: same refusal.
        let resolver =
            ScriptedResolver::answering(vec![addr("10.0.0.1", 443), addr("93.184.216.34", 443)]);
        let egress = EgressResolver::with_resolver(
            resolver,
            EgressAddressPolicy::EXTERNAL,
            DEFAULT_MAX_DNS_ANSWERS,
        );
        assert!(egress.resolve_vetted("mixed.example", 443).await.is_err());
    }

    #[tokio::test]
    async fn global_answer_is_vetted_and_resolved_exactly_once() {
        let resolver = ScriptedResolver::answering(vec![
            addr("93.184.216.34", 443),
            addr("2606:4700::1111", 443),
        ]);
        let egress = EgressResolver::with_resolver(
            resolver.clone(),
            EgressAddressPolicy::EXTERNAL,
            DEFAULT_MAX_DNS_ANSWERS,
        );
        let vetted = egress.resolve_vetted("cdn.example", 443).await.unwrap();
        assert_eq!(vetted.len(), 2);
        assert_eq!(resolver.calls(), 1, "exactly one DNS resolution");
        assert_eq!(resolver.seen_hosts(), vec!["cdn.example".to_string()]);
    }

    #[tokio::test]
    async fn answer_set_bound_is_enforced_and_never_truncated() {
        let limit = 4usize;
        let answers: Vec<SocketAddr> = (1..=limit + 1)
            .map(|i| SocketAddr::from(([93, 184, 216, i as u8], 443)))
            .collect();
        let resolver = ScriptedResolver::answering(answers);
        let egress = EgressResolver::with_resolver(resolver, EgressAddressPolicy::EXTERNAL, limit);
        match egress.resolve_vetted("flood.example", 443).await {
            Err(EgressResolveError::TooManyAnswers { count, limit: l }) => {
                assert_eq!(count, limit + 1);
                assert_eq!(l, limit);
            }
            other => panic!("expected the answer bound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_and_failed_lookups_are_typed_and_refused() {
        let resolver = ScriptedResolver::answering(Vec::new());
        let egress = EgressResolver::with_resolver(
            resolver,
            EgressAddressPolicy::EXTERNAL,
            DEFAULT_MAX_DNS_ANSWERS,
        );
        assert_eq!(
            egress.resolve_vetted("empty.example", 443).await,
            Err(EgressResolveError::NoAddresses)
        );
        let egress = EgressResolver::with_resolver(
            ScriptedResolver::failing(),
            EgressAddressPolicy::EXTERNAL,
            DEFAULT_MAX_DNS_ANSWERS,
        );
        assert_eq!(
            egress.resolve_vetted("dead.example", 443).await,
            Err(EgressResolveError::LookupFailed)
        );
        // Host bounds: empty and over-long names never reach the resolver.
        let resolver = ScriptedResolver::answering(vec![addr("8.8.8.8", 443)]);
        let egress = EgressResolver::with_resolver(
            resolver.clone(),
            EgressAddressPolicy::EXTERNAL,
            DEFAULT_MAX_DNS_ANSWERS,
        );
        assert_eq!(
            egress.resolve_vetted("", 443).await,
            Err(EgressResolveError::EmptyHost)
        );
        let long = "a".repeat(MAX_DNS_HOST_CHARS + 1);
        assert!(matches!(
            egress.resolve_vetted(&long, 443).await,
            Err(EgressResolveError::HostTooLong { .. })
        ));
        assert_eq!(resolver.calls(), 0, "invalid hosts are never looked up");
    }

    #[test]
    fn resolve_error_display_never_carries_resolver_text() {
        let err = EgressResolveError::AddressClassRefused {
            host: "allowed.example".into(),
            class: AddressClass::LinkLocal,
        };
        let rendered = format!("{err}");
        assert!(rendered.contains("link_local"), "{rendered}");
        let debug = format!("{err:?}");
        assert!(!debug.contains("dns exploded"));
    }
}
