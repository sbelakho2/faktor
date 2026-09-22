# Faktor Acquire — Commerce Source

Normative requirements for the internal `Faktor Acquire` system and its
normalized commerce layer, `Faktor Commerce Source`. This document is the
implementation contract; CI and review treat it as binding.

## 1. Purpose

One model-facing tool, `source_market`, backed by a multi-backend acquisition
runtime (not a monolithic scraper). For any requested datum the runtime picks
the strongest available acquisition path:

1. fresh local cache
2. official API
3. stable first-party JSON/HTTP response
4. browser network/XHR/CDP extraction
5. embedded page/application state
6. semantic DOM extraction
7. rendered-text fallback
8. typed human-verification requirement

## 2. Hard invariants

* `source_market` not invoked ⇒ 0 scraper network requests, 0 browser
  launches, 0 marketplace API calls, 0 marketplace DB work beyond opening,
  0 internally triggered model calls.
* An unrelated Faktor turn ⇒ 0 `source_market` schema tokens.
* If `source_market` is invoked, the acquisition system itself still makes
  **zero LLM calls**. `commerce_internal_model_calls_total` must permanently
  equal 0.
* Crate dependency graph for `faktor-acquire`, `faktor-commerce`,
  `faktor-commerce-connectors`, `faktor-browser` must not include
  `faktor-agent` reasoning runtime, `faktor-router` model execution, or any
  model adapter (openai/anthropic/google/ollama). CI enforces this.
* Every external response is hostile input: bounded sizes, nesting,
  redirects, node counts, variant matrices, compression.
* All acquired text is untrusted data: it enters as tool provenance, never
  as policy/instruction authority (prompt-injection boundary).
* Exact money only — no f64 anywhere in commerce paths.

## 3. Crate layout

```
crates/acquire/                 faktor-acquire (generic acquisition; no commerce vocabulary)
crates/browser/                 faktor-browser   (Chromium/CDP authority; no site knowledge)
crates/commerce/                faktor-commerce  (domain, matching, quotes, store, jobs)
crates/commerce-connectors/     faktor-commerce-connectors (site adapters only)
crates/cli/src/tools_market.rs  thin Faktor Tool gateway
```

`faktor-acquire` knows nothing about SKU/MPN/MOQ/price breaks/suppliers.
`faktor-browser` knows nothing about specific sites. Site knowledge lives
only in connector modules.

## 4. Lazy tool exposure (faktor-agent)

```rust
pub enum ToolExposure {
    Normal,
    Lazy { phases: PhaseMask, triggers: Vec<ToolTrigger> },
}

pub struct ToolRegistry {
    tools: HashMap<String, Arc<Tool>>,
    exposure: HashMap<String, ToolExposure>,
}

registry.register(tool);                     // Normal (unchanged)
registry.register_lazy(tool, exposure);
registry.bundle_for_phase(phase, caps);      // unchanged, empty lazy set
registry.bundle_for_phase_with_activation(phase, caps, &activation);
```

Deterministic activation, no classifier/embedding model. Strong signals:
`1688`, `1688.com`, `Alibaba`, `Alibaba.com`, `LCSC`, `Mouser`, `DigiKey`,
`supplier`, `sourcing`, `source this part`, `BOM pricing`,
`component pricing`, `buy 5,000`, `find manufacturers`, `quote this BOM`,
and product URLs (`https://detail.1688.com/...` activates immediately).
Explicit session flag `/source on`. Prefer false negatives over false
positives (discussing a Rust `supplier` trait must not activate).

`source_market` phases when active: Plan/Explore/Retrieve/Implement yes;
Review usually no; TestAnalysis/Summarize/Compact/Title/Embed no; Debug
optional. `resource_class: Network`.

## 5. The single tool

```json
{
  "type": "object",
  "properties": {
    "op": { "enum": ["search", "product", "quote", "bom", "job"] },
    "sources": {
      "type": "array", "maxItems": 6,
      "items": { "enum": ["auto", "1688", "alibaba", "lcsc", "mouser", "digikey"] }
    },
    "q": { "type": "string", "maxLength": 512 },
    "ref": { "type": "string", "maxLength": 2048 },
    "qty": { "type": "integer", "minimum": 1, "maximum": 1000000000 },
    "limit": { "type": "integer", "minimum": 1, "maximum": 50 },
    "freshness": { "enum": ["prefer_cache", "live", "cache_only"] },
    "detail": { "enum": ["compact", "normal", "full"] },
    "items": {
      "type": "array", "maxItems": 500,
      "items": {
        "type": "object",
        "properties": {
          "q": { "type": "string", "maxLength": 512 },
          "qty": { "type": "integer", "minimum": 1 }
        },
        "required": ["q", "qty"],
        "additionalProperties": false
      }
    },
    "job_id": { "type": "string", "maxLength": 128 }
  },
  "required": ["op"],
  "additionalProperties": false
}
```

Rust performs per-operation validation. No giant nested `oneOf` schemas.
Operation semantics: `search` = discovery only; `product` = inspect one
known ref; `quote` = real applicable price at qty; `bom` = many items;
`job` = deterministic state/result of a bulk job. The model needs no
site-specific knowledge.

## 6. Acquisition planner

`AcquisitionPlanner` receives `AcquisitionRequest`, `ConnectorCapabilities`,
`ConnectorHealth`, `CredentialAvailability`, `QuotaState`, `CacheState`,
`RequestedFreshness`, `RequestedFields` and produces `AcquisitionPlan`.
Connectors advertise capabilities (discovery, exact_product,
quantity_pricing, stock, packaging, account_pricing, supplier_data, bulk)
and mechanisms (`OfficialApi`, `DirectHttp`, `BrowserNetwork`,
`EmbeddedState`, `Dom`). Adding new marketplaces must not touch the planner.

## 7. Canonical model (faktor-commerce)

* `ProductIdentity { manufacturer, manufacturer_part_number,
  source_part_number, offer_id, canonical_url, category }`
* `CommercialOffer { source, identity, title, description, currency,
  price_breaks, variants, moq, order_multiple, standard_pack, stock,
  lead_time, packaging, supplier, manufacturer, provenance,
  observed_at_ms }`
* `Money { currency, micros: i64 }` (¥0.0037 = 3700 micro-CNY; wire format
  decimal string `"0.003700"`, never JSON float)
* `PackagingOption` first-class (Cut Tape, Tape & Reel, Digi-Reel, Tray,
  Tube, Bulk, Full Reel, Factory pack) with own MOQ/multiple/stock/prices.
* `VariantOffer { variant_id, attributes, stock, price_breaks }`;
  ambiguous variant mapping ⇒ `status: variant_ambiguous`, never silently
  pick the cheapest SKU.
* `price_at_quantity(offer, variant, quantity) -> QuoteResolution` handling
  MOQ, order multiple, package quantity, packaging, variant, tiers,
  explicit promotions, account pricing — deterministic, no model math.
* `MatchCertainty { Exact, Strong, Possible, Ambiguous, Mismatch }` plus
  machine-readable signals (`mpn_exact`, `manufacturer_exact`,
  `package_match`, `suffix_difference`), never a bare score.
* `PartNumberNormalizer` preserves meaningful suffixes (reel/tray, tolerance,
  voltage, temperature grade); `TPS5430DDAR` and other packaging suffixes are
  materially different purchasable items.
* Ranking is a transparent deterministic weighted function (exact MPN,
  manufacturer, package, quantity compatibility, stock, MOQ, multiple,
  lifecycle, price completeness, source confidence, seller completeness;
  marketplaces add title token overlap, variant/spec overlap, supplier
  evidence). No hidden LLM.
* `Quote { merchandise, shipping, tax, duty, total }` — unknown shipping is
  `None`, never 0.
* `PriceVisibility { Public, Authenticated, AccountSpecific, Promotional,
  InquiryRequired, Unknown }`; RFQ/contact-supplier must be
  `price: null, price_visibility: "inquiry_required"`, never fabricated.
* `Freshness { Live, FreshCache{age_ms}, StaleFallback{age_ms} }` — stale is
  never presented as current.

## 8. Multi-extractor reconciliation

`Observation<T> { value, origin, observed_at_ms }` with origins OfficialApi,
HttpJson, BrowserNetwork, EmbeddedState, StructuredMarkup, Dom,
RenderedText. `ResolvedField<T>` is `Value{value, authority,
corroborated_by}`, `Conflict{observations}`, or `Missing`. Never average
conflicting prices; record the conflict and apply documented authority
ordering. Extraction health is tracked per source/backend/field; strategies
report success/not-applicable/schema-mismatch/conflict and the runtime
remembers which strategy works (self-optimization without an LLM).
Structural fingerprints (JSON key signatures, DOM landmarks, script-state
schema digest) classify `SchemaDrift` instead of silently misparsing.

## 9. Browser authority (faktor-browser)

Chromium/CDP from Rust: dedicated launch or attach, persistent profiles,
incognito contexts, navigation, DOM access, JS eval, network events with
bodies, cookies inside the profile boundary, downloads disabled by default,
screenshots only on request. Extraction priority: XHR/fetch JSON →
GraphQL/structured network → application state → JSON script blocks →
structured metadata → semantic DOM → text-anchored DOM → CSS last resort.
Multi-extractor reconciliation as in §8.

Mandatory local proxy: Chromium gets only `127.0.0.1:<broker-port>`; the
Faktor Browser Egress Broker does destination filtering (per-connector
first-party policies), request accounting, upstream proxy selection,
credential isolation, logging/redaction, health. Chromium never receives
upstream proxy credentials. Request interception aggressively discards
video/audio/ads/tracking/images/marketing/fonts/WebRTC unless needed.
No anti-bot bypass subsystem: no CAPTCHA solving, fingerprint spoofing,
challenge bypass, per-block IP rotation, cookie theft. On verification:
detect → stop that profile → `VerificationRequired` → human completes in a
dedicated headed profile → resume same profile. Profiles live under
`<data-dir>/commerce/profiles/<name>` with restrictive permissions;
cookies never enter CAS and are never model-visible. Lazy browser startup
with idle shutdown (default several minutes); `max_browsers` bounded.
Chromium launches only through the existing `ProcessSupervisor` with
`ProcessOwner::Browser { source, profile }` and an `EnvSpec` allowlist —
never the daemon environment (no provider keys, tokens, API secrets,
`FAKTOR_SERVER_PASSWORD`, proxy passwords). Stable `BrowserIdentity
{ account, profile, egress }`; egress changes only for operational reasons.

Honest isolation semantics: today's process-level network isolation is not
an OS sandbox; use broker+interception and document it. Do not silently
downgrade strict sandbox guarantees.

## 10. Connector contract

```rust
#[async_trait]
pub trait CommerceConnector {
    fn source(&self) -> SourceId;
    fn capabilities(&self) -> ConnectorCapabilities;
    async fn discover(&self, ctx: &AcquireCtx, req: SearchRequest) -> Result<Vec<Discovery>, SourceError>;
    async fn product(&self, ctx: &AcquireCtx, req: ProductRequest) -> Result<CommercialOffer, SourceError>;
    async fn quote(&self, ctx: &AcquireCtx, req: QuoteRequest) -> Result<Vec<QuoteCandidate>, SourceError>;
}
```

Connectors never create their own HTTP client or Chromium process; both are
injected. All HTTP goes through the existing checked egress authority
(`Arc<dyn HttpTransport>`) with destination validation, sandbox network
policy, secret scanning, request/response bounds.

Site strategy (asymmetric by design):

| Site | Primary | Secondary | Tertiary |
|------|---------|-----------|----------|
| Mouser | Official Search API | product page/network | DOM |
| DigiKey | Product Information V4 (KeywordSearch discovery; ProductDetails / PricingOptionsByQuantity for live price) | product page/network | DOM |
| LCSC | Official API (KeywordSearch → ProductDetails) | first-party JSON | browser |
| 1688 | Open Platform when scope suffices | authenticated browser network | embedded state → DOM |
| Alibaba.com | Open API when scope suffices | browser network | DOM |

Respect documented API limits (Mouser 30/min, 1000/day; LCSC 200/min,
1000/day baseline; DigiKey `X-RateLimit-Limit`/`X-RateLimit-Remaining`
headers). Browser fallback is for legitimate field availability/resilience,
never quota evasion; API and browser fallback are independently
circuit-broken. API-first: do not scrape around official interfaces.

API credentials live outside the model: config references env var names;
values registered with the outbound secret scanner; never in schema, args,
`ToolOutcome`, model context, traces, or browser command lines.

## 11. Cache and store

Separate `<data-dir>/commerce/commerce.db` (SQLite WAL) with strict
migrations. Tables: `product_identity`, `source_product`, `offer_snapshot`,
`variant_snapshot`, `price_break`, `stock_snapshot`, `supplier`,
`supplier_snapshot`, `search_cache`, `connector_state`,
`browser_profile_state`, `egress_state`, `job`, `job_item`, `challenge`.
Indexes center on normalized MPN, manufacturer, source SKU, offer id, cache
expiry, job digest, observed timestamp.

Cache identity includes commercial context (source, product/offer, account,
locale, market, currency, quantity, packaging, variant, pricing scope);
account-specific data is never shared across account scopes. Field-level
freshness (supplier/description slow; search moderate; stock/price fast);
conditional HTTP (ETag/If-None-Match, Last-Modified/If-Modified-Since; 304
means no parse/no duplicate snapshot). Persist normalized snapshots with
provenance, extractor/connector/normalization versions, content digest, and
small diagnostics — never raw HTML, all XHR bodies, screenshots, cookies, or
auth headers (debug capture opt-in, bounded, sanitized). Request coalescing
by normalized acquisition identity (1 external request, N awaiters).

## 12. Jobs

`CommerceJob { id, digest, state, request }` with
`BLAKE3(normalized request + enabled sources + profile identity + freshness
policy)`; identical requests attach to the same active job. Jobs do
deterministic work only (no model calls), are restart-safe, and can return
`{status: "running", job_id}` at the tool deadline. Large results go to a
CAS artifact with a compact context result (target 4–12 KiB, hard max
64 KiB): `{status, lines, matched, ambiguous, unmatched, artifact,
important[]}`. Artifacts never contain cookies, OAuth refresh tokens, API
secrets, proxy secrets, session headers, or CSRF tokens. Privacy-safe
logging: query digest, source, operation, counts, status, latency — never
customer BOMs.

## 13. Config (strict; unknown keys are startup errors)

```json
{
  "commerce": {
    "enabled": true,
    "database": "commerce.db",
    "cache": { "discovery_ttl_s": 1800, "product_ttl_s": 21600,
               "price_ttl_s": 1800, "stock_ttl_s": 900,
               "supplier_ttl_s": 86400 },
    "browser": { "enabled": true, "executable": null, "headless": true,
                 "idle_shutdown_s": 300, "max_browsers": 2,
                 "max_pages_per_profile": 1 },
    "connectors": {
      "1688": { "enabled": true, "profile": "procurement-cn" },
      "alibaba": { "enabled": true, "profile": "procurement-global" },
      "lcsc": { "enabled": true, "api_key_env": "FAKTOR_LCSC_KEY" },
      "mouser": { "enabled": true, "api_key_env": "FAKTOR_MOUSER_KEY" },
      "digikey": { "enabled": true, "client_id_env": "FAKTOR_DIGIKEY_CLIENT_ID",
                   "client_secret_env": "FAKTOR_DIGIKEY_CLIENT_SECRET" }
    }
  }
}
```

Disabled parity: `{}` and `{"commerce": {"enabled": false}}` ⇒ no commerce
tool registered, no commerce DB created, no API client state, no Chromium,
no profile directory, no broker, no network request, no schema tokens; the
rest of Faktor renders byte-identical requests.

## 14. Daemon integration and CLI

`DaemonGraph.commerce: Option<Arc<CommerceSourceService>>`, constructed
after memory/tokenizers and before `agent`/`orchestrator` (the tool needs
the Arc before the final ToolRegistry is injected). Update
`DAEMON_CONSTRUCTION_ORDER` and structural graph tests. The service is not
constructed inside the tool factory.

`crates/cli/src/tools_market.rs` is a thin gateway: bounded argument
validation, typed request construction, capability permission check, service
invocation, compact result serialization, `ToolOutcome` assembly. No SQL,
HTTP, browser, parsing, credentials, or rate limiting in the tool.

Admin commands (never model tools): `faktor commerce doctor`, `status`,
`login <source>`, `logout <source>`, `clear-cache`. Login opens the headed
dedicated browser; the model never handles passwords.

## 15. Errors, health, retries, limits

* `SourceError` typed variants: Disabled, InvalidRequest,
  AuthenticationRequired, VerificationRequired{kind}, RateLimited
  {retry_after_ms}, QuotaExhausted{reset_ms}, CoolingDown{until_ms},
  EgressUnavailable, NetworkTimeout, ApiUnavailable, BrowserUnavailable,
  BrowserCrashed, ProductNotFound, VariantAmbiguous, ExtractionIncomplete,
  ExtractionConflict, ResponseTooLarge, Cancelled, Deadline, Store.
  Never a generic "scrape failed".
* `ConnectorHealth`: Healthy, Degraded{reason}, CoolingDown{until_ms},
  RateLimited{until_ms}, AuthenticationRequired, VerificationRequired
  {challenge}, QuotaExhausted{reset_ms}, Unavailable. The planner sees it.
* Transient retries (connect reset, API 502) happen below the agent with
  bounded jitter; auth/CAPTCHA/quota surface immediately.
* Adaptive throttling: honor Retry-After; success restores throughput;
  transient errors reduce it. Multi-scope rate keys: global/source/account/
  profile/egress/host/operation-class (Discovery, Product, Supplier, Login,
  Static). Normal authenticated profile: 1 page navigation at a time.
* Deadlines and cancellation propagate from `ToolRunCtx` through service,
  planner, API futures and browser navigation. Single quotes should fit the
  ordinary tool budget; large BOMs use jobs.
* Hard bounds everywhere: query length, BOM lines, HTTP bytes, JSON
  nesting, DOM size, captured network bodies, candidates, variants, price
  tiers, redirects, browser pages, concurrent requests, artifact size.

## 16. Testing and certification

Fixtures per extraction strategy (API, browser network, DOM, schema drift,
malformed, hostile), replayable offline in CI. Acceptance tests must catch
plausible wrong prices (headline range ¥2.00–¥40.00 with requested variant
¥36.00 must never quote ¥2.00; cached KeywordSearch price must never
override live ProductDetails when live pricing was requested). Security:
credential leakage, malicious redirects, hostile HTML/JSON, unexpected
destinations, prompt injection, cross-account cache isolation. Token
economics (release-blocking): 10,000 ordinary coding turns with commerce
enabled but unused ⇒ 0 schema appearances, 0 extra input tokens, 0 model
calls, 0 HTTP calls, 0 browser launches; a Mouser price question ⇒ exactly
one `source_market` invocation, 0 internal model calls; a 500-line BOM ⇒
one initiating interaction, deterministic job, full result in artifact,
compact context result, no hidden summarizer. Long-run soak, live canary
(opt-in, `[soak]`/ignored per repo convention) and browser-crash/quota
exhaustion/restart-recovery suites.

## 17. Build order (normative)

1. lazy tool activation in faktor-agent (byte-identical wire plans for
   unrelated prompts)
2. faktor-commerce domain types (money, identity, packaging, variants,
   tiers, stock, MOQ, provenance, freshness, quote resolution)
3. deterministic matching + quote resolution with exhaustive tests
4. faktor-acquire (bounded HTTP, cancellation, deadlines, typed retry,
   cache policy, quota, health, coalescing, provenance)
5. commerce.db (migrations, snapshots, caches, jobs, connector/profile
   state, GC)
6. Mouser connector (reference; official API)
7. DigiKey connector (KeywordSearch discovery, ProductDetails /
   PricingOptionsByQuantity live pricing, rate-limit headers)
8. LCSC connector (KeywordSearch → ProductDetails, discounted prices)
9. faktor-browser (supervised Chromium, profiles, CDP capture, semantic
   DOM, interception, cancellation, bounds)
10. Browser Egress Broker (mandatory local proxy, destination policy,
    credential confinement, traffic filtering, auditing)
11. 1688 Open Platform adapter, then browser acquisition (network →
    embedded state → structural DOM → verification last)
12. Alibaba Open API capabilities, then browser acquisition (separate
    extraction logic)
13. connector/backend circuit breakers, per-field extraction health,
    structural fingerprints + schema-drift detection, adaptive rate control
14. deterministic BOM jobs + request coalescing (restart-safe)
15. single lazy `source_market` tool (thin gateway)
16. CommerceSourceService in the daemon graph exactly once; construction
    order tests updated
17. admin login/status/doctor commands
18. security, robustness (fixture replay), canary, soak, and
    token-economics certification suites
