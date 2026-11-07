# landing/

Input material for the landing page of tidepool and for all other user-facing
copy. Nothing here is part of the build or of the test suite.

The page itself is `website/.vitepress/theme/Landing.vue`. It is the home page
of the doc site, at the root of propell.dev/tidepool.

| | |
| --- | --- |
| [`copy-style.md`](copy-style.md) | the binding style guide for all user-facing text: the positioning, and the Simplified Technical English rules. Read it before you write copy. |
| [`product-and-copy.md`](product-and-copy.md) | the positioning brief: what tidepool is, who it is for, the inventory, the facts and numbers to quote, what tidepool does not do, and the structure of the landing page. |
| [`visual-language.md`](visual-language.md) | the styling brief: palette, typography, geometry and motion. All of it comes from `frontend/style.css` and the running UI. |
| [`screenshots/`](screenshots) | screenshots of the web UI, captured against `examples/retail` with `tidepool feed` running. The table of contents is at the end of `visual-language.md`. |

The screenshots are 1600×1000 CSS px at 2× device scale, in the light and the
dark theme (`-light.png`, `-dark.png`).

To take them again:

1. Run `just build`.
2. Run `./target/release/tidepool init target/shots`. Remove `data_dir` from
   its `tidepool.yaml`.
3. Run `./target/release/tidepool serve --config target/shots`.
4. In another terminal, run `./target/release/tidepool feed`.
5. Capture each page at 1600×1000, device scale 2, once with
   `prefers-color-scheme: light` and once with `dark`, and with reduced
   motion, so that no value is caught in its flash (Playwright:
   `browser.newContext({ viewport, deviceScaleFactor: 2, colorScheme,
   reducedMotion: 'reduce' })`). For `02`, click `east` in the region
   filter. Save `02` as JPEG (quality 82) to `website/public/screens/` too.
