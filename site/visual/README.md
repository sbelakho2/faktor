# Visual review record

The PDF edition is the design source. These images are the reviewed evidence:

- `pdf/page-02.webp` … `pdf/page-57.webp` — every content page of the PDF
  edition, including all prior-edition pages, at the 4:3 review aspect.
- `pdf-reference.webp` — contact sheet of all page references.
- `parity/` — side-by-side PDF page / web implementation montages for all
  designed page groups: home, approach, documentation, evidence, outcomes,
  workflow, patterns, identity, research, editor, prior, prior-approach and
  prior-identity.
- `pdf-structure.tsv` — the page-by-page copy manifest checked by
  `cargo test -p faktor-site` against the built HTML.

`baselines/` holds first-screen screenshots of the implementation at the three
review widths (1440×1080, 1024×768, 390×844), captured with the pinned
Playwright Chromium. `scripts/site-visual.sh` rebuilds the site on a temporary
port, recaptures those screens, and compares each with the reviewed baseline by
RMSE (default tolerance 0.02). It also compares every designed page group with
its PDF reference and regenerates the side-by-side montages when
`UPDATE_VISUAL_BASELINES=1`.

Reviewed against the PDF on 24 October 2026:

- Exact palette, type families, type sizes, gutters and breakpoints are
  asserted in `crates/site/src/tests.rs` and `faktor-design-tokens.json`.
- Home, approach, documentation, evidence, outcomes, workflow, patterns,
  identity, research, editor and all three prior-edition page groups were
  compared in the parity montages.
- The PDF chrome is exact: it has no auth links. `/login/`, `/signup/`,
  `/plans/` and `/account/` remain reachable by their designed routes.
- The prior edition is duplicated page by page: prior home (7 screens), prior
  approach (3 screens) and prior identity (5 screens) with the dated banner,
  prior navigation and prior footer.
- The account, auth, billing, invoice, usage, security and admin surfaces have
  no PDF page; they are reviewed against the same token system and are covered
  by route tests rather than this screenshot gate.

Refresh deliberately:

```sh
UPDATE_VISUAL_BASELINES=1 scripts/site-visual.sh
scripts/site-visual.sh
```
