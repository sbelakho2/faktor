# Historical UI attribution (retained license texts only)

Source: <https://github.com/Kilo-Org/kilocode>

Earlier releases of this repository vendored upstream UI trees (the frozen
`packages/kilo-vscode/webview-ui` and `packages/kilo-ui` sources). Both trees
were **removed** by the Faktor-owned-UI migration: the VS Code extension now
ships its own hand-written panel (`apps/vscode/media/chat.js`, `chat.css`,
`composer-state.js`, `faktor.svg`), the JetBrains app ships its own Kotlin
panels, and no upstream UI source, bundle, overlay or manifest remains in the
tree.

The license texts below are retained **only as historical attribution** for
the removed MIT-licensed code, whose copyright notice and permission notice
accompanied that earlier distribution:

- `kilocode-LICENSE.txt` — upstream repository `LICENSE` (covered `kilo-ui`).
- `kilo-vscode-LICENSE.txt` — upstream `packages/kilo-vscode/LICENSE` (covered
  the vendored `webview-ui`).

No Faktor product code is derived from those trees; this directory is the ONE
location where the retired product name is allowed to appear, and the CI
source/artifact scan (`tests/static-authority` + `scripts/branding-scan.sh`)
prohibits it everywhere else.
