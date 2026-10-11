# Marketplace acquisition: legal and compliance posture

Status: normative operator note for the Faktor Acquire marketplace
connectors (Mouser, DigiKey, LCSC, 1688, Alibaba.com). It describes what the
code does and what the operator must ensure before enabling a source. It is
not legal advice; per-source counsel and vendor agreements decide.

Relevant implementation surfaces: `docs/acquire.md` (site strategy),
`crates/commerce-connectors` (the connectors), `crates/browser` (the
browser authority and stored profiles), `crates/cli/src/config/commerce.rs`
(the strict `[commerce]` section), the opt-in live canaries
(`crates/commerce-connectors/tests/live_canary.rs`), and the doctor
live-canary line (`faktor-cli doctor`).

## 1. Scope and authorization

- Only the operator's own accounts, credentials and browser sessions may be
  used. The operator is responsible for holding a vendor agreement or
  license that permits automated access with those credentials.
- Official APIs come first by construction: discovery and exact product
  lookup go to the documented API endpoints
  (`docs/acquire.md` §3). The browser path exists for legitimate field
  availability and resilience, never to route around an official interface
  that would answer the same question.
- The connectors carry no anti-bot bypass subsystem. There is no CAPTCHA
  solving, no fingerprint spoofing, no IP rotation, no cookie theft, and no
  challenge circumvention anywhere in the codebase (`crates/browser`,
  crate-level non-negotiables).
- Text acquired from a marketplace is DATA with a data origin; it can never
  become an instruction to the agent (fixture-proven by the security
  certification suite).

## 2. Terms of service and robots posture

- API usage is bound by each vendor's API terms: rate limits, caching rules,
  attribution, and storage restrictions. The connectors implement the
  documented limits (Mouser 30/min + 1000/day; LCSC 200/min + 1000/day
  baseline) and DigiKey additionally consumes the live
  `X-RateLimit-Limit`/`X-RateLimit-Remaining`/`Retry-After` headers, so the
  hard-coded numbers are never the only limit. `429` answers surface as the
  typed `RateLimited` state with the vendor's retry-after.
- Browser acquisition is scoped by a per-connector first-party destination
  policy: the browser can only reach the connector's own marketplace hosts
  through the local egress broker; third-party hosts are refused
  (`DestinationBlocked`). No connector widens another connector's policy.
- `robots.txt` and any vendor automation policy are honored by not
  bypassing them: the browser authority presents an ordinary client, keeps
  cookies inside the dedicated profile, and stops the profile's automated
  work at the first verification interstitial. Where a vendor's terms
  prohibit browser automation entirely, the operator must disable that
  connector (see §6) and rely on the official API only, if one exists.
- API and browser fallback are independently circuit-broken; a mechanism is
  never silently switched, and a browser call is never made to evade an
  exhausted API quota (`docs/acquire.md` §10). The planner selects the
  mechanism; adapters execute exactly that mechanism.

## 3. Rate limits, retries and load posture

- Requests are quota-governed through the shared `QuotaState` per source,
  with documented limits registered at construction.
- Retries use typed, bounded backoff; no unbounded request storms. The
  browser path uses lazy Chromium startup with idle shutdown, bounded pages
  per profile, and a bounded browser count.
- Live canaries are strictly opt-in and each performs a handful of
  requests; they are never part of the default CI run and never run without
  the operator's own credentials.

## 4. Stored sessions, cookies and privacy

- A stored logged-in session lives ONLY as a Chromium profile directory
  under `<data-dir>/commerce/profiles/<name>` with owner-only permissions
  (`0700`). The profile is created, removed and restricted through
  filesystem-handle-anchored primitives that refuse symlink swaps.
- Connectors never read cookie values. The browser authority keeps cookies
  inside the profile boundary and never sends them to the model; capture
  bounds exclude credential material from what crosses the seam.
- Interactive login (`commerce login`) is the only headed flow; automated
  acquisition uses the stored profile (or an incognito context when no
  login is needed).
- An operator can wipe a profile at any time (`faktor-browser` profile
  removal); wiping it ends all access to the vendor session.
- Account-scoped captures carry an explicit access authority
  (`AccessVisibility`): a price observed through a logged-in or
  account-scoped profile can never be presented as a public price, and
  cache rows never cross account scopes.
- The operator is the data controller for everything inside the data
  directory (profiles, commerce cache/database, optional canary evidence
  JSON). Retention, export and deletion are operator duties.

## 5. Human verification

- A detected login form, CAPTCHA, security slider, access-denied page or
  rate-limit page becomes a typed state. The profile's automated work stops;
  the connector returns `VerificationRequired` /
  `AuthenticationRequired` / `RateLimited` instead of retrying or evading.
- A human completes the verification in the headed browser window and the
  operator re-runs the acquisition. The system never completes a challenge
  on its own.
- The live browser canary treats a detected verification state as a
  PASSING, recorded outcome (`outcome=verification-required`): proof that
  the stop flow works, not a failure to route around.

## 6. Disabling a source (explicit, per source)

Every source is independently switchable; disabling is a first-class,
strict config decision:

- `[commerce] enabled = false` (default) disables the whole surface: no
  commerce tool, no database, no connector client, no browser profile, no
  network request.
- `commerce.connectors.<source>.enabled = false` disables one source. The
  resolved config is byte-identical to the absent key (disabled parity):
  a disabled connector is never constructed, performs no credential
  resolution and no client state exists.
- `commerce.browser.enabled = false` (default) keeps every browser
  mechanism off; a marketplace connector then requires its Open
  Platform/Open API credential (1688/Alibaba) or cannot be enabled at all.
- Credentials are referenced by environment-variable NAME only; an operator
  who removes the credential from the environment immediately removes the
  connector's ability to authenticate.

## 7. Live canary status and evidence

- The registry `faktor_commerce_connectors::canary::LIVE_CANARY_SOURCES` is
  the single contract for which environment variables gate each canary:
  `FAKTOR_LIVE_MOUSER_KEY`, `FAKTOR_LIVE_DIGIKEY_CLIENT_ID` +
  `FAKTOR_LIVE_DIGIKEY_CLIENT_SECRET`, `FAKTOR_LIVE_LCSC_KEY`,
  `FAKTOR_LIVE_1688_PROFILE` + `FAKTOR_LIVE_DATA_DIR`,
  `FAKTOR_LIVE_ALIBABA_PROFILE` + `FAKTOR_LIVE_DATA_DIR`.
- Nothing runs without those variables: each canary skips (with a printed
  line) and makes zero vendor requests. The canaries are `#[ignore]`-gated
  and assigned to the key-gated `live-paid` nightly lane; the default CI
  run never spends and never contacts a vendor.
- `faktor-cli doctor --config <path>` prints one informational line per
  source: `live-canary <source>: configured` when every required variable
  is present in the environment, else `live-canary <source>: absent`.
  Absence is the default and is never an error.
- Each canary asserts structural validity (shapes and domain invariants,
  never captured vendor constants) and records a BLAKE3 evidence digest
  over the secret-scrubbed structural summary, plus a bounded
  `faktor-live-canary/v1` evidence line; setting
  `FAKTOR_LIVE_CANARY_EVIDENCE_DIR` additionally writes the record as JSON
  (it contains no credential: values are scrubbed with the same scanner
  used for outbound traffic).
- A canary failure is a signal to investigate the vendor change or the
  operator's credentials; it never weakens the offline fixture suites,
  which remain the authoritative regression gates.

## 8. Operator checklist

1. Confirm each enabled source is permitted by your vendor agreement.
2. Prefer official API credentials; enable the browser path only where the
   agreement and `robots`/automation policy allow it.
3. Keep canary/API credentials in the environment (or a secret manager that
   injects them); never commit them.
4. Confirm `faktor-cli doctor` reports the intended canary status per
   source and the intended `[commerce]` enablement.
5. Review the evidence digest lines/JSON after a canary run; investigate
   any change before trusting live vendor data.
6. Remember `commerce.connectors.<source>.enabled = false` and profile
   wipe are your immediate off-switches.
