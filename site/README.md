# Faktor website

The Faktor marketing site and account/billing surface. Zola renders the static
pages; the Axum service (`crates/site`) serves them and owns sessions, accounts,
organizations and the billing handoff. There is no JavaScript at runtime.

## Source of design

`Faktor_Website.pdf` in the repository root is the design source. The extracted
tokens are in `site/static/downloads/faktor-design-tokens.json`, the CSS
variables in `site/static/css/site.css`, and the PDF page references in
`site/visual/pdf/` (every content page, 2–57). Reviewed side-by-side
parity montages live in `site/visual/parity/`.

Core tokens:

| Token | Value |
| --- | --- |
| Ink | `#08110C` |
| Forest | `#143D2B` |
| Paper | `#F3F5EF` |
| Sage | `#92AA99` |
| Ochre | `#D9BA75` |
| Ochre Deep | `#72541E` |
| Display | Barlow Semi Condensed 600 |
| Reading | Barlow 400–600 |
| Metadata | IBM Plex Mono 400–500 |
| Desktop shell | 1440 px, 36 px gutters; 96 px page-hero inset |
| Breakpoints | 1180 / 1023 / 820 / 640 |
| Type scale | display 108 / 0.95, subpage title 108 / 0.95, H2 58, lead 22, body 18, mono 11 |

The editor prototype's host/theme/state controls are CSS-only radio state at
`/editor/`. The account and billing surfaces use the same identity but are not
depicted in the PDF; they are designed from the same tokens. The PDF-designed
chrome contains no auth links, so it stays exact: `/login/`, `/signup/`,
`/plans/` and `/account/` remain reachable by their designed routes rather than
being advertised in the top chrome or footer.

## Pages

- `/` home (the eight PDF screens: hero, plan/change/check, evidence, finish,
  work/inspect/history, outcomes/editor, task patterns, workspace/FAQ)
- `/approach/`, `/documentation/`, `/evidence/`, `/outcomes/`, `/workflow/`,
  `/patterns/`, `/identity/`, `/research/`, `/editor/`
- `/plans/`, `/terms/`, `/privacy/`
- `/prior/` (prior home, seven screens), `/prior/approach/` (three screens)
  and `/prior/identity/` (five screens) — complete page-by-page prior edition,
  with the dated banner, prior navigation, prior footer and every designed
  page/flow rather than dated summaries
- `/login/`, `/signup/`, `/forgot-password/`, `/reset-password/`,
  `/verify-email/`, `/logout/`
- `/account/` (overview/profile), `/account/organization/`, `/account/billing/`,
  `/account/invoices/`, `/account/invoices/{id}/`, receipt, CSV/JSON export,
  `/account/usage/`, `/account/security/`, `/admin/`
- `/webhooks/adyen`, `/healthz`

## Build and run

Prerequisites: Rust 1.98 (workspace toolchain), Zola 0.22.1. Playwright and
ImageMagick are only needed for the visual gate.

```sh
zola --root site build
FAKTOR_SITE_DATA_DIR=/tmp/faktor-site \
FAKTOR_SITE_BASE_URL=http://127.0.0.1:8080 \
FAKTOR_SITE_COOKIE_SECURE=0 \
  cargo run -p faktor-site -- serve
```

The server reads `site/public` by default. Build with the public deployment URL
when deploying:

```sh
zola --root site build --base-url https://faktor.example
```

For a production session cookie, use an HTTPS `FAKTOR_SITE_BASE_URL` (the cookie
then gets `Secure`) or set `FAKTOR_SITE_COOKIE_SECURE=1` explicitly.

## Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `FAKTOR_SITE_DATA_DIR` | `data/site` | SQLite database, payment database, outbox |
| `FAKTOR_SITE_STATIC_DIR` | `site/public` | Zola output served as the static site |
| `FAKTOR_SITE_BASE_URL` | `http://127.0.0.1:8080` | Canonical origin for links and cookies |
| `FAKTOR_SITE_BIND` | `127.0.0.1:8080` | Axum listener |
| `FAKTOR_SITE_COOKIE_SECURE` | derived from base URL | Force `Secure` cookies |
| `FAKTOR_SITE_TRUST_PROXY` | `0` | Trust `X-Forwarded-For`/`X-Real-IP` |
| `FAKTOR_SITE_ADMIN_EMAIL` | unset | Email bootstrapped as platform admin |
| `FAKTOR_SITE_BILLING_PROVIDER` | `local` | `local` or `adyen` |
| `FAKTOR_SITE_WEBHOOK_SECRET` | generated for local | Local webhook HMAC secret |
| `FAKTOR_SITE_PAYMENT_VAULT_KEY` | generated key file | Payment-token sealing key |
| `FAKTOR_ADYEN_BASE_URL` | `https://checkout-test.adyen.com/v71` | Adyen API base |
| `FAKTOR_ADYEN_MERCHANT_ACCOUNT` | required for Adyen | Adyen merchant account |
| `FAKTOR_ADYEN_API_KEY` | required for Adyen | Adyen API key (server-side only) |
| `FAKTOR_ADYEN_HMAC_KEY` | required for Adyen | Adyen webhook HMAC key |

Email verification and password-reset messages use a local, mode-0600 outbox
under `<data-dir>/outbox` by default. The `Mailer` trait is the production
transport seam: an SMTP implementation plugs in behind `Mailer` without
changing auth, templates or tests, while the local outbox remains the default
for development and CI. No SMTP credentials are invented or stored by this
repository. Adyen (and Stripe in `faktor-payments`) remain provider-configured
through their existing configuration surfaces; the website hard-codes neither
credentials nor provider behavior.

## Accounts and security

- Argon2id password hashes; passwords are 12–256 bytes.
- 32-byte session secrets, only SHA-256 hashes stored. Cookies are `HttpOnly`,
  `SameSite=Lax`, `Secure` in production, `Path=/`, with absolute (30 day) and
  idle (7 day) expiry. Login rotates the session; password reset and admin
  disable revoke sessions.
- Per-session CSRF secrets on every mutating form, plus an anonymous CSRF
  cookie for signup/login/recovery. Same-origin checks are applied as defence in
  depth.
- Bounded in-memory rate limits on login, signup, recovery, checkout, payment
  methods, admin actions and webhooks. Input and body bounds are enforced.
- Organization-scoped queries and an active organization stored on the session;
  switching verifies membership. Platform admins use a separate surface and get
  a 404 for non-admin disclosure. Billing reads use the payment service's
  organization scope.
- Security headers include a no-script CSP, `nosniff`, frame denial,
  no-referrer and a restrictive permissions policy.

## Billing

Plans are `solo` (€19/€190), `team` (€49/€490), and `enterprise`
(€199/€1,990), monthly/annual, EUR with exact minor units and 21% NL VAT. The
subscription, invoice and payment-method records live in the existing
`faktor-payments` service. The website stores only checkout intent/URL records.

Flow:

1. `POST /account/billing/checkout` creates the subscription and an open invoice
   in the payment service, then asks the provider for a hosted checkout with
   `local_invoice_id` bound into the provider reference.
2. The browser is redirected to the provider URL. A return to
   `/account/billing/checkout/return/` never settles the invoice.
3. The provider posts to `/webhooks/adyen`. The adapter verifies the HMAC,
   timestamp window and event id before the service applies payment state.
   Duplicate events return `[duplicate]`; invalid/stale signatures return 401.
4. Local mode renders `/account/billing/local/{id}/` and delivers a signed local
   provider event on the pay form, so the same verified-webhook path is exercised
   without an external account.

Payment methods accept only opaque provider tokens. A PAN-shaped value is
refused; no card fields exist in the schema or forms. Receipts render only the
recorded invoice state and never provider references.

## Gates

```sh
scripts/site-check.sh          # fmt, check, test, clippy, Zola, zero-JS, PDF structure
scripts/site-visual.sh         # 48 screenshots at 1440/1024/390
VISUAL_TOLERANCE=0.02 scripts/site-visual.sh
UPDATE_VISUAL_BASELINES=1 scripts/site-visual.sh   # review parity montages after
```

The visual gate compares first-screen screenshots to reviewed baselines
(RMSE <= 0.02) for every route and runs a structural PDF reference comparison
plus a side-by-side montage for all 13 designed page groups (RMSE <= 0.55;
`PDF_VISUAL_TOLERANCE` can tighten it). `crates/site` also validates
`site/visual/pdf-structure.tsv` page by page: every one of the 56 content PDF
pages must have its designed copy present, the PDF chrome must not add auth
links, and the prior routes must be full pages with the dated banner. The
account/billing pages are tested by the Rust route suite (signup/login/
verification, local and Adyen checkout, webhook HMAC/timestamp/idempotency,
tenant isolation, roles, admin, exact money), not by the PDF gate because they
have no PDF page.

## Deployment

`docker/site.Dockerfile` builds Zola and the Rust service in stages and runs the
binary with `/data` for durable SQLite state. The container is configured to
listen on `0.0.0.0:8080`; set `FAKTOR_SITE_BASE_URL` to the HTTPS origin and mount
`/data` on durable storage. `.woodpecker/site.yaml` adds the website gates to CI
without touching certification lanes.

Residual: production SMTP delivery (behind `Mailer`), a real Adyen merchant
account, and multi-instance rate-limit coordination.
