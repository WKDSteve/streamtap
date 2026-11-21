# CLAUDE.md

What an agent needs to know in this repository. The README says what streamtap is; this file says how to work in it.

## Layout

- `extension/`: the MV3 extension (Chrome `minimum_chrome_version` 116, Firefox 128 via `extension/scripts/build-firefox.mjs`).
  Plain scripts, no bundler, no Node dependency; its tests are `extension/scripts/test-*.mjs` (Node, fake DOM / fake
  `chrome`, no browser).
- `desktop/`: the Rust workspace (sink, host, core, testkit, the Tauri 2 app). The app's page is static files in
  `desktop/crates/streamtap-app/ui` (no build step); every colour comes from its `palette.css`.
- `tools/`: repository checks (Python) and the browser checks (`tools/page-check.mjs`; Playwright and axe at fixed
  versions from `tools/page-check`, the only `package.json` of the repository; nothing streamtap ships reads it).
- `TESTS/sheets`: testing.nvim sheets; how each app starts is in `.testing.lua`.

## Checks

What CI runs (`.github/workflows/ci.yml`), the parts that run on Linux:

- `cd desktop && cargo fmt --all --check && cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --locked`
- the `node` steps of the `extension` job (`node --check` on every script, `extension/scripts/test-*.mjs`,
  `desktop/scripts/test-app-ui.mjs`, ...)
- `pnpm --dir tools/page-check install --frozen-lockfile && node tools/page-check.mjs tools/page-check/node_modules`
- the hygiene job: `python -m unittest discover tools`, `tools/check-control-chars.py`, `tools/check-process-starts.py`,
  `tools/check-color-literals.py`, `tools/check-workflows.py`

A new process start belongs in `tools/process-starts.allow` and `docs/PROCESSES.md`. The workflows follow the rules of
`tools/check-workflows.py` (actions pinned by commit, a pull request never on the self-hosted runner).

## Commits

`type(scope): subject` in English (`fix(sink): ...`, `ci: ...`, `docs: ...`), a body that says why. Push to `main`.

## Web platform

- Browser Support, two targets:
  - the extension (`extension/`): Baseline 2023 (`extension/.mwgrc`), from its manifest minimums (Chrome 116,
    Firefox 128). Baseline 2023 also holds features that reached Chrome only in 117–120: use those only behind feature
    detection, so Chrome 116 keeps working (decided 2026-11-21). `extension/scripts/test-baseline.mjs` holds the twelve
    (web-features) and fails on a use without a `baseline-guard: <feature id>` mark; a marked use needs its fallback
    tested;
  - the app's page (`desktop/crates/streamtap-app/ui`, WebView2): Baseline Widely available (`.mwgrc` in the root).

  Neither UI has a `package.json`, so there is no `browserslist`. Newer features only behind feature detection with a
  fallback; polyfills where browsers still in use need them (rather one too many), run only after the check: dynamic `import()`, or bundled in a `file://` page (rules-vault `BASE-02`) (the extension loads no remote code: a polyfill there must ship in
  the extension).
- Before writing or changing HTML, CSS or client-side JS, use the `modern-web-guidance` skill (`search` → `retrieve` →
  verify). Not for Rust, backend, CI or build config. The tool finds `.mwgrc` from the current directory upwards and
  stops at the first one (or at the repository root): run it from `extension/` for extension work, from the root for
  the app's page.
- For open-ended modernisation: `npx -y modern-web-guidance@latest legacy-scan <ui-dir> --json` (`.` from
  inside `extension/`, `desktop/crates/streamtap-app/ui` from the root), check each finding against
  `keepWhen`/`dropWhen`. Known finding, tracked as `streamtap/settimeout-stellen-pruefen`: the `setTimeout` calls
  of the extension and the app's page; leave them to that task.
- Rules: rules-vault ruleset `web` (plus the app rules `WEB-*` in ruleset `native`).
- `.claude/settings.json` turns the tool's telemetry off and allows only `npx -y modern-web-guidance@latest *`. No CI scanner
  here (no package manager for the UIs); the scan runs locally and in agent sessions.
