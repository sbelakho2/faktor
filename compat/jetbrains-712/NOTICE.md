# compat/jetbrains-712 — pinned upstream JetBrains 7.1.2 source

This directory pins the upstream Kilo JetBrains plugin source released as
**7.1.2** so the frozen UI target is reproducible and hash-verified instead
of an external, unverifiable dependency.

| Field | Value |
| --- | --- |
| Upstream | `https://github.com/Kilo-Org/kilocode` |
| Tag | `jetbrains/v7.1.2` (tag commit `a29fc822e8d00f42c15f64372fdfc7c245f7c78d`) |
| Pinned commit | `436ff09e649bd0866c84bd9f98933a74cad2d25c` (`release(jetbrains): v7.1.2`; the first tree whose `gradle.properties` declares `kilo.jetbrains.version=7.1.2`) |
| Upstream path | `packages/kilo-jetbrains` → `compat/jetbrains-712/kilo-jetbrains` |
| License | MIT — `LICENSES/kilocode-LICENSE.txt` (repo root `LICENSE` at the pinned commit) |
| Files | 1043 files, 7,707,650 bytes |
| Hash manifest | `ui/upstream.json` → `jetbrains_712.file_hashes` (SHA-256 per file, relative to this directory) |
| Fetch protocol | `ui/upstream.json` → `jetbrains_712.fetch`; `scripts/vendor-upstream.sh` re-runs it (additive step) |
| Offline verification | `apps/jetbrains/frontend/src/test/kotlin/dev/faktor/frontend/JetBrainsParitySmoke.kt` (`UPSTREAM PIN PASS`) re-hashes every pinned file from `ui/upstream.json` with no network |

## Rendering decision (one implementation)

Upstream 7.1.2 is a Kotlin/Swing UI, not a web UI, so the JCEF + shared
Solid-bundle path is not the upstream shape. The Faktor-owned Swing panels
under `apps/jetbrains/frontend` are the **single** rendering implementation
(`native-swing-single-implementation`), talking Faktor Native Protocol v1;
this vendored tree is the pinned reference/build source, never a second
renderer. `ui/upstream.json` records the per-surface Faktor patch set under
`jetbrains_712.faktorPatchSet`.

## What this pin does and does not establish

- **Does**: the exact upstream 7.1.2 source is in-tree, MIT-attributed and
  per-file hash-verified offline; the parity surfaces it names are covered
  by the Faktor frontend's fixture/interaction tests.
- **Does not**: the upstream plugin itself is not built in this repository
  (its Gradle build resolves the IntelliJ Platform SDK and a pinned CLI
  release from the network; the local/CI smokes compile the Faktor sources
  with plain `kotlinc`). The Faktor plugin id/version and the running
  frontend stay Faktor-owned (`dev.faktor.jetbrains`, `0.1.0`).
