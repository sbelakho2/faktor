# Live DeepSeek configuration requirements

The live DeepSeek battery (`deepseek-chat`, `deepseek-reasoner`) exposed two
configuration facts that are **not** provider bugs. Both are enforced at graph
build time so a bad config fails before the first paid call.

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
