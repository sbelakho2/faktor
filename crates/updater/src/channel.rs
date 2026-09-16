//! Release channels and DETERMINISTIC channel selection.
//!
//! A release channel is a total order of trust: `dev` accepts every channel,
//! `beta` accepts beta and stable, `stable` accepts stable only. Selection
//! over several candidate manifests (a feed) is a pure function of the
//! candidates plus `now_ms`: expired candidates are dropped, non-accepted
//! channels are dropped, and the winner is the highest version, broken by
//! newer `issued_at`, then by the lexicographically greater commit, then by
//! input order. Two runs over the same candidates always pick the same one.

use crate::version::Version;

/// One release channel.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    Stable,
    Beta,
    Dev,
}

impl Channel {
    pub const ALL: &'static [Channel] = &[Channel::Stable, Channel::Beta, Channel::Dev];

    pub const fn as_str(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Beta => "beta",
            Channel::Dev => "dev",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|c| c.as_str() == raw)
    }

    /// Whether a manifest published on `candidate` may be consumed by a
    /// client configured for `self`.
    pub const fn accepts(self, candidate: Channel) -> bool {
        match self {
            Channel::Dev => true,
            Channel::Beta => matches!(candidate, Channel::Beta | Channel::Stable),
            Channel::Stable => matches!(candidate, Channel::Stable),
        }
    }
}

impl std::fmt::Display for Channel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The selection-relevant fields of one candidate manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelCandidate {
    pub channel: Channel,
    pub version: Version,
    pub commit: String,
    pub issued_at: i64,
    pub expires_at: i64,
}

impl ChannelCandidate {
    /// Candidates whose validity window covers `now_ms`.
    pub fn is_valid_at(&self, now_ms: i64) -> bool {
        now_ms <= self.expires_at && now_ms >= self.issued_at
    }
}

/// Pick the winner index among `candidates` for a `configured` channel, or
/// `None` when nothing is acceptable. See the module docs for the total
/// order; this function is pure and deterministic.
pub fn select(configured: Channel, candidates: &[ChannelCandidate], now_ms: i64) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (index, candidate) in candidates.iter().enumerate() {
        if !configured.accepts(candidate.channel) || !candidate.is_valid_at(now_ms) {
            continue;
        }
        best = match best {
            None => Some(index),
            Some(current) => {
                let incumbent = &candidates[current];
                let wins = candidate.version > incumbent.version
                    || (candidate.version == incumbent.version
                        && (candidate.issued_at > incumbent.issued_at
                            || (candidate.issued_at == incumbent.issued_at
                                && candidate.commit > incumbent.commit)));
                if wins {
                    Some(index)
                } else {
                    Some(current)
                }
            }
        };
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(channel: Channel, version: &str, issued_at: i64) -> ChannelCandidate {
        ChannelCandidate {
            channel,
            version: Version::parse(version).unwrap(),
            commit: "a".repeat(40),
            issued_at,
            expires_at: issued_at + 1_000,
        }
    }

    #[test]
    fn channel_trust_order_is_total() {
        assert!(Channel::Dev.accepts(Channel::Stable));
        assert!(Channel::Dev.accepts(Channel::Beta));
        assert!(Channel::Dev.accepts(Channel::Dev));
        assert!(Channel::Beta.accepts(Channel::Beta));
        assert!(Channel::Beta.accepts(Channel::Stable));
        assert!(!Channel::Beta.accepts(Channel::Dev));
        assert!(Channel::Stable.accepts(Channel::Stable));
        assert!(!Channel::Stable.accepts(Channel::Beta));
        assert!(!Channel::Stable.accepts(Channel::Dev));
        for channel in Channel::ALL {
            assert_eq!(Channel::parse(channel.as_str()), Some(*channel));
        }
        assert_eq!(Channel::parse("nightly"), None);
    }

    #[test]
    fn selection_is_deterministic_and_prefers_the_newest_compatible() {
        let candidates = vec![
            candidate(Channel::Stable, "2.0.0", 100),
            candidate(Channel::Beta, "2.1.0", 90),
            candidate(Channel::Dev, "2.2.0", 95),
        ];
        assert_eq!(select(Channel::Stable, &candidates, 150), Some(0));
        assert_eq!(select(Channel::Beta, &candidates, 150), Some(1));
        assert_eq!(select(Channel::Dev, &candidates, 150), Some(2));
        // Every run over the same inputs picks the same winner.
        for _ in 0..8 {
            assert_eq!(select(Channel::Dev, &candidates, 150), Some(2));
        }
        // Tie on version: newer issued_at wins; tie on both: greater commit
        // wins; identical entries: input order.
        let tied = vec![
            candidate(Channel::Stable, "3.0.0", 10),
            candidate(Channel::Stable, "3.0.0", 20),
        ];
        assert_eq!(select(Channel::Stable, &tied, 50), Some(1));
        let mut same = tied.clone();
        same[0].issued_at = 20;
        same[0].commit = "b".repeat(40);
        assert_eq!(select(Channel::Stable, &same, 50), Some(0));
    }

    #[test]
    fn expired_and_future_candidates_are_dropped() {
        let mut expired = candidate(Channel::Stable, "9.9.9", 0);
        expired.expires_at = 10;
        let fresh = candidate(Channel::Stable, "1.0.0", 0);
        let candidates = vec![expired, fresh];
        assert_eq!(select(Channel::Stable, &candidates, 50), Some(1));
        assert_eq!(select(Channel::Stable, &candidates, 5_000), None);
        // A candidate issued in the future is not yet selectable.
        let future = candidate(Channel::Stable, "1.0.0", 1_000);
        assert_eq!(select(Channel::Stable, &[future], 0), None);
    }
}
