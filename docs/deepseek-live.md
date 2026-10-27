# Live DeepSeek configuration requirements

The live DeepSeek battery (`deepseek-chat`, `deepseek-reasoner`) exposed two
configuration facts that are **not** provider bugs. Both are enforced at graph
build time so a bad config fails before the first paid call.

## Credential source

`providers[].api_key_env` reads the daemon process environment. An IDE
launched from a desktop icon does not inherit a shell export, so a key that
exists only in an operator file would otherwise reach the daemon as absent and
the first model call would fail as `Auth` (HTTP 401) after the run started.
Declare the file at the top level (the path is non-secret; the value stays in
the file):

```json
"provider_env_file": "/Users/me/.faktor-live/deepseek.env"
```

The daemon loads it in the boot preflight, BEFORE the provider secret registry
and adapters are built, so `api_key_env` resolves and the value is also
registered for output scrubbing. The file must be absolute, at most 64 KiB,
owner-only (`chmod 600`) on unix, and contain plain `KEY=VALUE` lines (an
optional `export ` prefix, matching quotes, and full-line `#` comments are
accepted). A variable already present in the process environment is never
overwritten. Never put the key itself in `faktor-plus.json`; a missing,
malformed, oversized or group/world-readable file is a typed startup refusal.

## Pricing origin

- Economy and Balanced only admit models whose effective price is known
  (`Quote` or `ConservativeCeiling`). `Unknown` rows are excluded.
- `billing_origin` is decided by the **endpoint**, not the wire family. An
  `open_ai` provider pointed at the canonical DeepSeek API
  (`https://api.deepseek.com` or `https://api.deepseek.com/v1`) is billed at
  the `OfficialDeepSeek` origin, so the catalog's documented ceiling applies.
- A model the catalog has no price for (for example `deepseek-reasoner` at the
  time of writing) stays `Unknown` even at an official endpoint. Declare a
  ceiling explicitly when using it under Economy/Balanced:

```json
"pricing": { "pricing_ceiling_micro_usd_per_million_tokens": 2000000 }
```

- An Economy/Balanced graph with configured models but **zero** admitted
  candidates now refuses to boot with the model list and the `Unknown pricing`
  cause; it no longer boots and dies later as a generic `NoCapableModel`.

## Quality floors

High-risk contracts escalate the required quality floor to 80. A user
declaration of 70 is authoritative and is **not** silently raised, so such a
contract fails with `NoCapableModel` by design. Do one of:

- declare at least 80 for the dimensions the work requires;
- omit the per-model `quality` block and let the catalog prior apply (the
  built-in DeepSeek prior is 90); or
- keep the low declaration and route only low/medium-risk work to it.

The router never silently substitutes a higher declaration; the config carries
the authority.
