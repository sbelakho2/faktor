# Fixtures

- `providers/` — provider behavior fixtures (capability probes, wire shapes);
  locked by `tests/integration/tests/provider_fixtures.rs`; see
  `providers/README.md` for the consuming adapter tests
- no screenshot corpus is committed: the visual compatibility suite renders
  its panel corpus at test time (`tests/visual` `render_panel`, bounded
  `PANEL_WIDTH`x`PANEL_HEIGHT`) and verifies the zero-pixel-diff contract
  outside branding masks in `tests/visual/tests/pixel_regression.rs`
- `repositories/` — synthetic fixture repositories for indexing/search tests
