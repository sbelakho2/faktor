//! Request interception (spec §9): CDP `Fetch` interception with a
//! per-connector destination policy. Video, audio, ads, tracking, images,
//! marketing pixels, fonts and other non-essential resource types are
//! discarded by default; nothing is ever "worked around" — a blocked
//! request is failed with `BlockedByClient`, not retried.

use serde_json::{json, Value};

use crate::cdp::CdpClient;
use crate::egress::{decide_resource, BlockReason, DestinationDecision, DestinationPolicy};
use crate::error::BrowserError;
use faktor_core::cancellation::CancellationToken;
use faktor_core::time::Deadline;

/// CDP resource types.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ResourceType {
    Document,
    Stylesheet,
    Image,
    Media,
    Font,
    Script,
    TextTrack,
    Xhr,
    Fetch,
    Prefetch,
    EventSource,
    WebSocket,
    Manifest,
    SignedExchange,
    Ping,
    CspViolationReport,
    Preflight,
    #[default]
    Other,
}

impl ResourceType {
    pub fn parse(raw: &str) -> Self {
        match raw {
            "Document" => ResourceType::Document,
            "Stylesheet" => ResourceType::Stylesheet,
            "Image" => ResourceType::Image,
            "Media" => ResourceType::Media,
            "Font" => ResourceType::Font,
            "Script" => ResourceType::Script,
            "TextTrack" => ResourceType::TextTrack,
            "XHR" => ResourceType::Xhr,
            "Fetch" => ResourceType::Fetch,
            "Prefetch" => ResourceType::Prefetch,
            "EventSource" => ResourceType::EventSource,
            "WebSocket" => ResourceType::WebSocket,
            "Manifest" => ResourceType::Manifest,
            "SignedExchange" => ResourceType::SignedExchange,
            "Ping" => ResourceType::Ping,
            "CSPViolationReport" => ResourceType::CspViolationReport,
            "Preflight" => ResourceType::Preflight,
            _ => ResourceType::Other,
        }
    }

    pub fn as_cdp(self) -> &'static str {
        match self {
            ResourceType::Document => "Document",
            ResourceType::Stylesheet => "Stylesheet",
            ResourceType::Image => "Image",
            ResourceType::Media => "Media",
            ResourceType::Font => "Font",
            ResourceType::Script => "Script",
            ResourceType::TextTrack => "TextTrack",
            ResourceType::Xhr => "XHR",
            ResourceType::Fetch => "Fetch",
            ResourceType::Prefetch => "Prefetch",
            ResourceType::EventSource => "EventSource",
            ResourceType::WebSocket => "WebSocket",
            ResourceType::Manifest => "Manifest",
            ResourceType::SignedExchange => "SignedExchange",
            ResourceType::Ping => "Ping",
            ResourceType::CspViolationReport => "CSPViolationReport",
            ResourceType::Preflight => "Preflight",
            ResourceType::Other => "Other",
        }
    }

    /// The default drop set: media, images, fonts, text tracks, speculative
    /// prefetches, manifests, pings and unknown resource types. Documents,
    /// stylesheets, scripts, XHR/fetch (the extraction path) and WebSockets
    /// stay unless the connector narrows the set.
    pub fn default_blocked() -> Vec<ResourceType> {
        vec![
            ResourceType::Image,
            ResourceType::Media,
            ResourceType::Font,
            ResourceType::TextTrack,
            ResourceType::Prefetch,
            ResourceType::Manifest,
            ResourceType::Ping,
            ResourceType::CspViolationReport,
            ResourceType::Other,
        ]
    }
}

/// One interception decision for a paused request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterceptionDecision {
    Continue,
    Block { reason: BlockReason },
}

impl InterceptionDecision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, InterceptionDecision::Continue)
    }
}

/// Interception counters (bounded, monotonic).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InterceptionStats {
    pub paused_total: u64,
    pub continued_total: u64,
    pub blocked_total: u64,
    pub blocked_by_reason: Vec<(BlockReason, u64)>,
}

/// The interception policy: destination policy + resource-type drops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterceptionPolicy {
    pub destinations: DestinationPolicy,
    /// Drop WebSocket resource types too (off by default).
    pub block_websockets: bool,
}

impl InterceptionPolicy {
    pub fn new(destinations: DestinationPolicy) -> Self {
        Self {
            destinations,
            block_websockets: false,
        }
    }

    pub fn with_block_websockets(mut self, block: bool) -> Self {
        self.block_websockets = block;
        self
    }

    /// Decide one paused request. The destination policy's resource-type
    /// blocklist and host rules both apply; deny wins.
    pub fn decide(&self, url: &str, resource_type_raw: &str) -> InterceptionDecision {
        let resource_type = ResourceType::parse(resource_type_raw);
        if self.block_websockets && resource_type == ResourceType::WebSocket {
            return InterceptionDecision::Block {
                reason: BlockReason::ResourceTypeBlocked,
            };
        }
        match decide_resource(&self.destinations, url, resource_type) {
            DestinationDecision::Allowed => InterceptionDecision::Continue,
            DestinationDecision::Blocked { reason } => InterceptionDecision::Block { reason },
        }
    }

    /// The CDP `Fetch.enable` patterns: intercept every request at the
    /// request stage so the decision function sees each one. (Pattern-level
    /// filtering cannot express the host+type matrix without duplicating the
    /// policy; the decision runs in one place.)
    pub fn fetch_patterns(&self) -> Value {
        json!([{ "urlPattern": "*", "requestStage": "Request" }])
    }
}

/// A live interceptor bound to one CDP session.
pub struct Interceptor {
    client: CdpClient,
    session: String,
    policy: InterceptionPolicy,
    stats: std::sync::Mutex<InterceptionStats>,
}

impl Interceptor {
    pub fn new(client: CdpClient, session: impl Into<String>, policy: InterceptionPolicy) -> Self {
        Self {
            client,
            session: session.into(),
            policy,
            stats: std::sync::Mutex::new(InterceptionStats::default()),
        }
    }

    pub fn policy(&self) -> &InterceptionPolicy {
        &self.policy
    }

    pub fn stats(&self) -> InterceptionStats {
        self.stats.lock().unwrap().clone()
    }

    /// The CDP command that enables interception on this session.
    pub fn enable_command(&self) -> (&'static str, Value) {
        (
            "Fetch.enable",
            json!({ "patterns": self.policy.fetch_patterns() }),
        )
    }

    /// Handle one `Fetch.requestPaused` event: continue allowed requests and
    /// fail blocked ones. Returns the decision for the caller/tests.
    pub async fn handle_request_paused(
        &self,
        params: &Value,
        deadline: Deadline,
        cancel: &CancellationToken,
    ) -> Result<InterceptionDecision, BrowserError> {
        let request_id = params
            .get("requestId")
            .and_then(Value::as_str)
            .ok_or_else(|| BrowserError::cdp("Fetch.requestPaused without requestId"))?
            .to_string();
        let url = params
            .get("request")
            .and_then(|request| request.get("url"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let resource_type = params
            .get("resourceType")
            .and_then(Value::as_str)
            .unwrap_or("Other");
        let decision = self.policy.decide(&url, resource_type);
        {
            let mut stats = self.stats.lock().unwrap();
            stats.paused_total = stats.paused_total.saturating_add(1);
            match &decision {
                InterceptionDecision::Continue => {
                    stats.continued_total = stats.continued_total.saturating_add(1);
                }
                InterceptionDecision::Block { reason } => {
                    stats.blocked_total = stats.blocked_total.saturating_add(1);
                    match stats
                        .blocked_by_reason
                        .iter_mut()
                        .find(|(r, _)| r == reason)
                    {
                        Some((_, count)) => *count = count.saturating_add(1),
                        None => stats.blocked_by_reason.push((*reason, 1)),
                    }
                }
            }
        }
        match &decision {
            InterceptionDecision::Continue => {
                self.client
                    .send(
                        Some(&self.session),
                        "Fetch.continueRequest",
                        json!({ "requestId": request_id }),
                        deadline,
                        cancel,
                    )
                    .await?;
            }
            InterceptionDecision::Block { reason } => {
                tracing::debug!(
                    host = %crate::capture::redact_url(&url),
                    resource_type = %resource_type,
                    reason = %reason,
                    "interception: request failed by policy"
                );
                self.client
                    .send(
                        Some(&self.session),
                        "Fetch.failRequest",
                        json!({ "requestId": request_id, "errorReason": "BlockedByClient" }),
                        deadline,
                        cancel,
                    )
                    .await?;
            }
        }
        Ok(decision)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egress::HostPattern;

    fn base_policy() -> InterceptionPolicy {
        InterceptionPolicy::new(DestinationPolicy::first_party_only(vec![
            HostPattern::parse("first.test").unwrap(),
        ]))
    }

    #[test]
    fn blocked_resource_types_are_dropped_even_on_first_party_hosts() {
        let policy = base_policy();
        assert_eq!(
            policy.decide("https://first.test/hero.png", "Image"),
            InterceptionDecision::Block {
                reason: BlockReason::ResourceTypeBlocked
            }
        );
        assert_eq!(
            policy.decide("https://first.test/ad.mp4", "Media"),
            InterceptionDecision::Block {
                reason: BlockReason::ResourceTypeBlocked
            }
        );
        assert_eq!(
            policy.decide("https://first.test/app.js", "Script"),
            InterceptionDecision::Continue
        );
        assert_eq!(
            policy.decide("https://first.test/api.json", "XHR"),
            InterceptionDecision::Continue
        );
        assert_eq!(
            policy.decide("https://first.test/page", "Document"),
            InterceptionDecision::Continue
        );
    }

    #[test]
    fn non_first_party_and_tracking_hosts_are_blocked() {
        let policy = base_policy();
        assert_eq!(
            policy.decide("https://tracker.test/pixel", "XHR"),
            InterceptionDecision::Block {
                reason: BlockReason::NotFirstParty
            }
        );
        assert_eq!(
            policy.decide("https://ads.test/x", "Script"),
            InterceptionDecision::Block {
                reason: BlockReason::NotFirstParty
            }
        );
        let strict = base_policy().with_block_websockets(true);
        assert_eq!(
            strict.decide("wss://first.test/socket", "WebSocket"),
            InterceptionDecision::Block {
                reason: BlockReason::ResourceTypeBlocked
            }
        );
    }

    #[test]
    fn unknown_resource_type_strings_parse_to_other_and_are_dropped() {
        assert_eq!(ResourceType::parse("SomethingNew"), ResourceType::Other);
        assert_eq!(
            base_policy().decide("https://first.test/x", "SomethingNew"),
            InterceptionDecision::Block {
                reason: BlockReason::ResourceTypeBlocked
            }
        );
    }

    #[test]
    fn fetch_patterns_intercept_the_request_stage() {
        let patterns = base_policy().fetch_patterns();
        assert_eq!(patterns[0]["urlPattern"], "*");
        assert_eq!(patterns[0]["requestStage"], "Request");
    }
}
