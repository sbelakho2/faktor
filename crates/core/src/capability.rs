//! Sandbox capabilities and permission decisions. Permissions are expressed
//! as capabilities, never as scattered `if provider == ...` conditionals.

use std::path::PathBuf;

/// A concrete capability request. The sandbox (faktor-sandbox) maps these to
/// `PermissionDecision` using session policy.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "capability", content = "detail", rename_all = "snake_case")]
pub enum Capability {
    ReadWorkspace { path: PathBuf },
    WriteWorkspace { path: PathBuf },
    ReadExternal { path: PathBuf },
    WriteExternal { path: PathBuf },
    ExecuteShell { command: String },
    Network { destination: String },
    Mcp { server: String },
    Git { operation: String },
}

impl Capability {
    pub fn describe(&self) -> String {
        match self {
            Capability::ReadWorkspace { path } => format!("read {path:?}"),
            Capability::WriteWorkspace { path } => format!("write {path:?}"),
            Capability::ReadExternal { path } => format!("read external {path:?}"),
            Capability::WriteExternal { path } => format!("write external {path:?}"),
            Capability::ExecuteShell { command } => format!("execute `{command}`"),
            Capability::Network { destination } => format!("network {destination}"),
            Capability::Mcp { server } => format!("MCP {server}"),
            Capability::Git { operation } => format!("git {operation}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    Allow,
    Deny,
    /// Ask the user through the frozen permission dialog.
    Ask,
}

/// Atomic capability classes for typed permission scopes (audit P0-40: the
/// hook `permission_scope` was an unchecked free-form String; scopes and
/// envelopes are now a typed lattice over these classes, mirroring the
/// [`Capability`] variants minus their per-request detail).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityKind {
    Read,
    Write,
    Execute,
    Network,
    Git,
    Mcp,
}

impl CapabilityKind {
    pub const ALL: [CapabilityKind; 6] = [
        CapabilityKind::Read,
        CapabilityKind::Write,
        CapabilityKind::Execute,
        CapabilityKind::Network,
        CapabilityKind::Git,
        CapabilityKind::Mcp,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            CapabilityKind::Read => "read",
            CapabilityKind::Write => "write",
            CapabilityKind::Execute => "execute",
            CapabilityKind::Network => "network",
            CapabilityKind::Git => "git",
            CapabilityKind::Mcp => "mcp",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        CapabilityKind::ALL
            .iter()
            .find(|k| k.as_str() == s)
            .copied()
    }
}

/// A typed capability set with lattice (subset) order: any finite subset of
/// [`CapabilityKind`], with `ALL` as the top element. Subset checks are the
/// permission-surface gate — a hook whose declared scope exceeds the
/// granted envelope is refused BEFORE any child is spawned. An unknown
/// capability id fails closed at deserialization (never assumed allowed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CapabilitySet(u8);

impl CapabilitySet {
    /// The empty scope: claims nothing, allowed under every envelope.
    pub const EMPTY: Self = Self(0);
    /// The full set — every known capability class.
    pub const ALL: Self = Self(0b0011_1111);

    pub const fn empty() -> Self {
        Self::EMPTY
    }

    pub const fn all() -> Self {
        Self::ALL
    }

    pub const fn of(kind: CapabilityKind) -> Self {
        Self(1 << kind as u8)
    }

    pub const fn contains(self, kind: CapabilityKind) -> bool {
        self.0 & (1 << kind as u8) != 0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn from_kinds(kinds: &[CapabilityKind]) -> Self {
        let mut mask = 0u8;
        for k in kinds {
            mask |= 1 << *k as u8;
        }
        Self(mask)
    }

    pub fn kinds(self) -> impl Iterator<Item = CapabilityKind> {
        CapabilityKind::ALL
            .iter()
            .copied()
            .filter(move |k| self.contains(*k))
    }

    /// Lattice order: `self` is within `other` when every claimed class is
    /// granted there. `ALL ⊄` any finite set; every set is within `ALL`.
    pub const fn is_subset_of(self, other: Self) -> bool {
        self.0 & !other.0 == 0
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }
}

impl Default for CapabilitySet {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl std::fmt::Display for CapabilitySet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if *self == Self::ALL {
            return f.write_str("*");
        }
        let mut first = true;
        for k in self.kinds() {
            if !first {
                f.write_str(",")?;
            }
            first = false;
            f.write_str(k.as_str())?;
        }
        Ok(())
    }
}

impl serde::Serialize for CapabilitySet {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> serde::Deserialize<'de> for CapabilitySet {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = CapabilitySet;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a capability scope: \"*\", \"read,write\", or an array of class names")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                if v == "*" {
                    return Ok(CapabilitySet::ALL);
                }
                let mut out = CapabilitySet::EMPTY;
                for part in v.split(',').map(str::trim) {
                    if part.is_empty() {
                        continue;
                    }
                    let k = CapabilityKind::from_name(part)
                        .ok_or_else(|| E::custom(format!("unknown capability class {part:?}")))?;
                    out = out.union(CapabilitySet::of(k));
                }
                Ok(out)
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                let mut out = CapabilitySet::EMPTY;
                while let Some(s) = seq.next_element::<String>()? {
                    let k = CapabilityKind::from_name(&s).ok_or_else(|| {
                        serde::de::Error::custom(format!("unknown capability class {s:?}"))
                    })?;
                    out = out.union(CapabilitySet::of(k));
                }
                Ok(out)
            }
        }
        d.deserialize_any(V)
    }
}

/// Network sandbox policy.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum NetworkPolicy {
    /// Everything blocked.
    DenyAll,
    /// Only provider endpoints listed in config.
    AllowProviders { endpoints: Vec<String> },
    /// Provider endpoints plus explicitly configured domains.
    AllowConfigured {
        endpoints: Vec<String>,
        domains: Vec<String>,
    },
}

impl NetworkPolicy {
    /// NON-AUTHORITATIVE (singular-authority law, audit P2): this raw
    /// matcher must not gate egress anywhere. Production egress
    /// authorization lives in the canonical destination/DNS authority;
    /// `allows` is retained only for non-authoritative classification and
    /// tests, and is deprecated so any new production use is a warning.
    ///
    /// TRUE when `destination` is admitted. Matching is ANCHORED on the URL
    /// authority (scheme + host + port) and a `/`-segment path boundary
    /// (audit P2-API): a configured `https://api.example.com` admits
    /// `https://api.example.com/v1` but never `https://api.example.com.evil`
    /// nor a different scheme/port. Raw prefix matching is deliberately
    /// retired — it admitted attacker-controlled string continuations.
    /// Anything unparseable fails closed.
    #[deprecated(
        note = "non-authoritative: production egress uses the canonical destination/DNS authority"
    )]
    pub fn allows(&self, destination: &str) -> bool {
        match self {
            NetworkPolicy::DenyAll => false,
            NetworkPolicy::AllowProviders { endpoints } => endpoints
                .iter()
                .any(|rule| authority_allows(rule, destination)),
            NetworkPolicy::AllowConfigured { endpoints, domains } => endpoints
                .iter()
                .chain(domains.iter())
                .any(|rule| authority_allows(rule, destination)),
        }
    }
}

/// Parse one absolute HTTP(S)-style URL into `(scheme, host, port, path)`.
/// `None` for anything malformed, non-http(s), or carrying userinfo — the
/// strict parse is part of the policy (an unparsed destination is denied).
fn split_authority(url: &str) -> Option<(String, String, Option<u16>, String)> {
    let (scheme, rest) = url.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    let scheme = scheme.to_ascii_lowercase();
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, path) = rest.split_at(end);
    if authority.is_empty() || authority.contains('@') || authority.contains(' ') {
        return None;
    }
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let close = rest.find(']')?;
        let host = &rest[..close];
        let after = &rest[close + 1..];
        let port = if after.is_empty() {
            None
        } else {
            Some(after.strip_prefix(':')?.parse().ok()?)
        };
        (host.to_ascii_lowercase(), port)
    } else if let Some((host, port)) = authority.rsplit_once(':') {
        (host.to_ascii_lowercase(), Some(port.parse().ok()?))
    } else {
        (authority.to_ascii_lowercase(), None)
    };
    if host.is_empty() {
        return None;
    }
    let path = if path.is_empty() {
        "/".to_string()
    } else {
        path.to_string()
    };
    Some((scheme, host, port, path))
}

fn default_port(scheme: &str) -> u16 {
    if scheme == "https" {
        443
    } else {
        80
    }
}

/// TRUE when `destination` matches `rule` at the authority boundary and, when
/// the rule names a path, at a '/'-segment boundary (`/v1` admits `/v1` and
/// `/v1/x`, never `/v10`).
fn authority_allows(rule: &str, destination: &str) -> bool {
    let Some((rule_scheme, rule_host, rule_port, rule_path)) = split_authority(rule) else {
        return false;
    };
    let Some((dest_scheme, dest_host, dest_port, dest_path)) = split_authority(destination) else {
        return false;
    };
    if rule_scheme != dest_scheme || rule_host != dest_host {
        return false;
    }
    if rule_port.unwrap_or_else(|| default_port(&rule_scheme))
        != dest_port.unwrap_or_else(|| default_port(&dest_scheme))
    {
        return false;
    }
    let rule_path = rule_path.trim_end_matches('/');
    if rule_path.is_empty() {
        return true;
    }
    dest_path == rule_path
        || dest_path.starts_with(&format!("{rule_path}/"))
        || dest_path.starts_with(&format!("{rule_path}?"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(deprecated)] // non-authoritative classification matcher, tests only
    fn network_policy_matrix() {
        let deny = NetworkPolicy::DenyAll;
        assert!(!deny.allows("https://api.openai.com/v1"));
        let providers = NetworkPolicy::AllowProviders {
            endpoints: vec!["https://api.openai.com".into()],
        };
        assert!(providers.allows("https://api.openai.com/v1/chat"));
        // Authority-anchored matching (audit P2-API): continuations that a
        // prefix comparison would admit are refused.
        assert!(!providers.allows("https://api.openai.com.evil/v1"));
        assert!(!providers.allows("https://evilapi.openai.com/v1"));
        assert!(!providers.allows("http://api.openai.com/v1"));
        assert!(!providers.allows("https://api.openai.com:8443/v1"));
        assert!(!providers.allows("https://api.openai.com@evil.test/v1"));
        assert!(!providers.allows("https://evil.test/?next=https://api.openai.com"));
        assert!(!providers.allows("not-a-url"));
        let configured = NetworkPolicy::AllowConfigured {
            endpoints: vec!["https://api.anthropic.com".into()],
            domains: vec!["https://mcp.example.com".into()],
        };
        assert!(configured.allows("https://api.anthropic.com/v1/messages"));
        assert!(configured.allows("https://mcp.example.com"));
        assert!(configured.allows("https://mcp.example.com/a/b"));
        assert!(
            !configured.allows("https://mcp.example.com.evil/x"),
            "the host component is anchored, not prefixed"
        );
        assert!(!configured.allows("https://example.com"));
        // A path-carrying rule is anchored at a '/' segment boundary.
        let path_rule = NetworkPolicy::AllowProviders {
            endpoints: vec!["https://api.openai.com/v1".into()],
        };
        assert!(path_rule.allows("https://api.openai.com/v1/chat"));
        assert!(path_rule.allows("https://api.openai.com/v1"));
        assert!(!path_rule.allows("https://api.openai.com/v10"));
        assert!(!path_rule.allows("https://api.openai.com/v2"));
        // Explicit ports and IPv6 brackets are authority components too.
        let port_rule = NetworkPolicy::AllowProviders {
            endpoints: vec!["https://[::1]:8443".into()],
        };
        assert!(port_rule.allows("https://[::1]:8443/x"));
        assert!(!port_rule.allows("https://[::1]/x"));
    }

    #[test]
    fn capability_describe_is_nonempty_and_carries_detail() {
        for cap in [
            Capability::ReadWorkspace { path: ".".into() },
            Capability::WriteWorkspace { path: ".".into() },
            Capability::ReadExternal {
                path: "/etc".into(),
            },
            Capability::WriteExternal {
                path: "/etc".into(),
            },
            Capability::ExecuteShell {
                command: "rm -rf /".into(),
            },
            Capability::Network {
                destination: "https://x".into(),
            },
            Capability::Mcp {
                server: "fs".into(),
            },
            Capability::Git {
                operation: "push".into(),
            },
        ] {
            assert!(!cap.describe().is_empty());
        }
    }

    #[test]
    fn capability_json_tagging_roundtrip() {
        let cap = Capability::ExecuteShell {
            command: "cargo test".into(),
        };
        let v = serde_json::to_value(&cap).unwrap();
        assert_eq!(v["capability"], "execute_shell");
        let back: Capability = serde_json::from_value(v).unwrap();
        assert_eq!(back, cap);
        // unknown tags rejected
        let bad = serde_json::json!({"capability": "own_the_server", "detail": {}});
        assert!(serde_json::from_value::<Capability>(bad).is_err());
    }

    #[test]
    fn permission_decisions_serialize_stable() {
        assert_eq!(
            serde_json::to_string(&PermissionDecision::Ask).unwrap(),
            "\"ask\""
        );
        assert_eq!(
            serde_json::to_string(&PermissionDecision::Allow).unwrap(),
            "\"allow\""
        );
        assert_eq!(
            serde_json::to_string(&PermissionDecision::Deny).unwrap(),
            "\"deny\""
        );
    }

    #[test]
    fn capability_set_lattice_orders_scopes() {
        let read = CapabilitySet::of(CapabilityKind::Read);
        let rw = read.union(CapabilitySet::of(CapabilityKind::Write));
        assert!(CapabilitySet::EMPTY.is_subset_of(CapabilitySet::EMPTY));
        assert!(CapabilitySet::EMPTY.is_subset_of(rw));
        assert!(read.is_subset_of(rw));
        assert!(!rw.is_subset_of(read), "write is not granted by {read}");
        assert!(rw.is_subset_of(CapabilitySet::ALL));
        assert!(!CapabilitySet::ALL.is_subset_of(rw));
        assert!(CapabilitySet::ALL.is_subset_of(CapabilitySet::ALL));
        assert!(rw
            .union(CapabilitySet::of(CapabilityKind::Mcp))
            .contains(CapabilityKind::Mcp));
    }

    #[test]
    fn capability_set_scope_serde_roundtrips_and_fails_closed_on_unknown() {
        let rw =
            CapabilitySet::of(CapabilityKind::Read).union(CapabilitySet::of(CapabilityKind::Write));
        assert_eq!(serde_json::to_string(&rw).unwrap(), "\"read,write\"");
        let back: CapabilitySet = serde_json::from_str("\"read,write\"").unwrap();
        assert_eq!(back, rw);
        assert_eq!(serde_json::to_string(&CapabilitySet::ALL).unwrap(), "\"*\"");
        assert_eq!(
            CapabilitySet::ALL,
            serde_json::from_str::<CapabilitySet>("\"*\"").unwrap()
        );
        assert_eq!(
            serde_json::to_string(&CapabilitySet::EMPTY).unwrap(),
            "\"\""
        );
        let arr: CapabilitySet = serde_json::from_str("[\"execute\",\"network\"]").unwrap();
        assert!(arr.contains(CapabilityKind::Execute));
        assert!(arr.contains(CapabilityKind::Network));
        assert!(!arr.contains(CapabilityKind::Read));
        // Unknown classes fail closed — never silently allowed.
        assert!(serde_json::from_str::<CapabilitySet>("\"read,own_the_server\"").is_err());
        assert!(serde_json::from_str::<CapabilitySet>("\"read\"").is_ok());
    }
}
