# Pinned upstream corpora

## jetbrains-712/

The pinned upstream JetBrains 7.1.2 split-mode source corpus
(`kilo-jetbrains`, tag `jetbrains/v7.1.2`, MIT). Its per-file SHA-256
hashes live in `ui/upstream.json` under `jetbrains_712` and are
re-verified offline by `JetBrainsParitySmoke` and by
`scripts/vendor-upstream.sh --check`.

The v7.5.6 wire-compatibility fixture corpus (`kilo-v756/`) was retired by
explicit owner decision together with the compatibility subsystem; the
vendored v7.5.6 frontend sources remain under `ui/kilo-v756-webview/` as a
pinned UI source dependency.
