# Kilo v7.5.6 compat corpus

This directory pins and exercises the REAL upstream v7.5.6 client.

## Vendored upstream client (`upstream-sdk/`)

`packages/sdk/js` (npm `@kilocode/sdk@7.5.6`) from
`https://github.com/Kilo-Org/kilocode` tag `v7.5.6`, commit
`fa02955bfa17b60e57e0d7406d200a73337472ee`, copied verbatim. The tree is
never edited; the test harness resolves its TypeScript `.js` specifiers
through a harness-side hook (`tests/compat/js/resolve-ts.mjs`).

- Pin + blake3 hash of every file: `upstream.json`.
- License (MIT, repo root at the pin): `LICENSES/kilocode-LICENSE.txt`.
- Re-fetch/re-hash: `vendor-sdk.sh` (records the exact fetch protocol).
- Offline verification: `cargo test -p faktor-tests-compat --lib upstream_manifest`.

## Golden traces (`sdk-traces/`)

One JSON group per surface. Each step records the unmodified client's
request bytes (method, path, query encoding, body, content-type, Basic
auth) and the daemon's response as it crossed the wire, plus the vendored
type that declares the expected shape (`sdk_type`), a `passing` or
`divergence` status, and an honest note. `divergence` steps additionally
lock the currently-missing SDK fields so the fixture fails the moment the
daemon becomes compatible (forcing a promotion + docs update).

Run the replay (Node >= 22.15; skips with an explicit message without it):

```sh
cargo test -p faktor-tests-compat --lib unmodified_upstream_client
```

Re-record the request/response goldens deliberately:

```sh
FAKTOR_COMPAT_FREEZE_TRACES=1 cargo test -p faktor-tests-compat --lib unmodified_upstream_client
```

Surface status and residual gaps are documented in `docs/wire-compat.md`.

## Legacy scaffold fixtures (top-level `*.json`)

`basic_auth.json`, `hello.json`, `messages_page.json`, `sse_frames.json`,
`wire_*.json`, … are the daemon's own scaffold wire goldens locked by
`faktor-protocol` tests. They are NOT the upstream SDK shapes; the
`sdk-traces/` corpus above is what measures the real client.
