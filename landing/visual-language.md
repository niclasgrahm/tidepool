# tidepool: visual language

Everything here is taken from the running product (`frontend/style.css`,
`frontend/src/plot.rs` and the screenshots in `screenshots/`). It describes
what tidepool looks like today, so the site can look like the same thing.
When the UI changes its look, change this file, `website/.vitepress/theme/`
and the screenshots with it.

---

## 1. The one-sentence brief

tidepool looks like **a dense, quiet tool**: Radix Themes' slate greys, one
teal accent, and one ruby that means "selected". The filter lists carry the
design. Everything else is calm.

If the site reads as "the page a team keeps open all day", it is right. If it
reads as a SaaS hero with gradients and glowing cards, it is wrong.

The site is not kayak's site with other colours. kayak is dark only, with
mono everywhere and square corners. tidepool is light and dark, set in the
system's sans, with small radii.

---

## 2. Palette

The scales are [Radix Colors](https://www.radix-ui.com/colors): `gray`
(slate), `teal`, `ruby`, `green`, `red`, `amber`, light and dark. The rules
never use a step directly. They use these names:

| token | step | light | dark | used for |
| --- | --- | --- | --- | --- |
| `--paper` | gray 2 / gray 1 | `#f9f9fb` | `#111113` | the page |
| `--panel` | gray 1 | `#fcfcfd` | `#141517` | the bars across the page |
| `--surface` | white / gray 2 | `#ffffff` | `#18191b` | cards, inputs, popovers |
| `--ink` | gray 12 | `#1c2024` | `#edeef0` | text |
| `--ink-soft` | gray 11 | `#60646c` | `#b0b4ba` | labels, secondary text, axes |
| `--line` | gray 5 | `#e0e1e6` | `#2e3135` | the edges of cards |
| `--sand` | gray 7 | `#cdced6` | `#43484e` | the edges of controls |
| `--kelp` | gray 4 | `#e8e8ec` | `#272a2d` | hover, quiet fills, dividers |
| `--tide` | teal 11 | `#008573` | `#0bd8b6` | accent text: links, a `safe` class |
| `--tide-fill` | teal 9 | `#12a594` | `#12a594` | accent marks: bars, lines, the primary button, the mark |
| `--tide-soft` | teal 3 | `#e0f8f3` | `#0d2d2a` | the measure behind a value, the active palette entry |
| `--anemone` | ruby 9 | `#e54666` | `#e54666` | **selected**, filled: a selected value, a bar, a brush |
| `--anemone-text` | ruby 11 | `#ca244d` | `#ff949d` | the text of a chip |
| `--anemone-soft` | ruby 3 | `#feeaed` | `#3a141e` | a chip, a node inside a selected branch |
| `--anemone-line` | ruby 6 | `#f8bfc8` | `#6f2539` | the edge of a chip |
| `--problem` | red 11 | `#ce2c31` | `#ff9592` | errors |

Green and amber are states, never decoration: the `Live` pill and a delta
up are green, `Paused`, a `rebuild` class and a warning are amber.

Rules:

- **Ruby means selected.** The UI has a few other uses: the flash of a
  value that just changed, the `n waiting` badge, a `destructive` class and
  a removed file. Do not add more. On the site, ruby appears where a replica
  or a screenshot shows a selection, and on the `destructive` badge.
- **Teal is the brand.** Links, the active nav entry, the primary button,
  the mark.
- **No gradients, no glows.** Surfaces are flat, with a 1px edge. Only what
  pops over the page (the palette, a tooltip, the date picker) has a shadow:
  `--shadow-pop`.
- Both themes are first-class. The UI follows `prefers-color-scheme`. The site
  has the VitePress toggle and the same two palettes.

### The three filter states

The most characteristic picture of tidepool. A value in a filter list is a
row, 24px high, with a radius of 3px and an 8px square mark before it:

| state | fill | mark | text |
| --- | --- | --- | --- |
| selected | `--anemone` | white | white |
| possible | none | `--tide-fill` | `--ink` |
| excluded | gray 2 | gray 7 | gray 9 |

A filter with a measure draws it as a bar behind each value, in
`--tide-soft` (white at 16% on a selected value). A tree adds a chevron and
indents by depth. A node inside a selected branch is `--anemone-soft` with
`--anemone-text`. A node with a selection below it gets a 6px ruby dot.

---

## 3. Typography

Radix Themes' stacks: the system's fonts. Nothing is fetched, so the server
works offline and nothing reflows when a font arrives.

- **Sans**, `--display` and `--text`: `-apple-system, BlinkMacSystemFont,
  "Segoe UI Variable Text", "Segoe UI", Roboto, Ubuntu, "Helvetica Neue",
  Arial, sans-serif`. Base size 13px, line height 1.45.
- **Mono**, `--mono`: `ui-monospace, "SF Mono", SFMono-Regular, Menlo,
  Consolas, "Liberation Mono", "Cascadia Mono", monospace`. Code, YAML, and
  the figures around the data: table cells, axis labels, a filter's counts,
  the epoch, a chip's field name. 10.5 to 12px.

Numbers are tabular (`font-variant-numeric: tabular-nums`) everywhere.

Sizes in the UI: body 13px; small 11 to 12px; visual and card titles 600
13px; page titles 600 20px; a KPI value 600, up to 32px (sized by its card).
A filter's title is 500 11px, uppercase, `letter-spacing: 0.04em`, in
`--ink-soft`. Headings have `letter-spacing: -0.01em`, more at large sizes.
The site needs larger sizes for the hero: the same sans, 700, with tighter
letter-spacing.

**Lowercase.** The brand is `tidepool`. Headings on the site are lowercase, by
the copy style. The UI itself uses sentence case for titles ("Sales
overview"), because they are the user's names.

---

## 4. Geometry and spacing

- **Controls**: 28px high (24px inside a bar), `border-radius: 4px`, a 1px
  `--sand` edge, `--surface` fill. The primary button is `--tide-fill` with
  white text.
- **Cards** (filters, visuals, status facts): `--surface`, a 1px `--line`
  edge, `border-radius: 6px`. Padding 8px for a filter, 10px 12px for a
  visual.
- **Popovers**: `border-radius: 8px` and `--shadow-pop`.
- **Chips**: 24px high, radius 4px, `--anemone-soft` with an
  `--anemone-line` edge. Pills (`999px`) only for the `Live` state and
  badges. Tags (`grain: day`, `sales.yaml`) are mono 10.5px on gray 3,
  radius 3px.
- **The dashboard**: a filter rail of 248px on the left, the grid beside it.
  Its columns and row height come from the dashboard's file. Gaps and page
  padding are 8px: dense, as a tool is.

---

## 5. The shapes that make up the UI

**The top bar** (`--panel`, a 1px `--line` below): the mark (a teal rounded
square with two white waves) and `tidepool` in 600 14px; the sections
(Dashboards, Model, Config, Status), the current one on `--kelp`; the ⌘K
search; the pulse: a green pill, `Live` and `epoch 812` in mono (amber when
`Paused`), and a pause button.

**The context bar** of a dashboard: the crumbs (`Dashboards / Sales
overview`, then the file name as a tag), back and forward, the selection as
chips (`customers.region` in mono, then `east`, then ×), `Clear all`, and at
the right `answered in 17 ms` and `Edit`.

**The rail**: filter lists, trees, date ranges and search boxes, each a card
with its uppercase title and its count (`1 of 6 · revenue`) in mono. Under the
rail, a legend of the three states.

**Visuals**: KPI (a large number, and the change against a comparison on a
green or red tint), bar (a selected bar ruby, the others teal; a second metric
gray 8), line (teal, with a 12% area under it; a second metric gray 9; a brush
in ruby at 10%), table (dense, numbers in mono, a `--tide-soft` bar behind
the measured column).

**Charts** come from `plot.rs` and `charts.rs`: gridlines in `--kelp`, the
baseline in gray 6, labels in mono 10.5px `--ink-soft`.

**The status bar** along the bottom: the connection, the snapshot, the rows
and the timings, in mono 11px.

**The config page**: files on the left, the editor in the middle (mono, YAML
coloured: keys teal 11, strings amber 11, numbers blue 11), the plan on the
right with class badges: `safe` (teal 3 fill, teal 11 text), `rebuild` (amber
3, amber 11), `destructive` (ruby 9, white).

**The relationship diagram** (model page): table cards on a dotted 20px grid,
lines routed between key rows, ends that show one and many, and teal arrows
for the way selections flow. A chosen line is ruby.

---

## 6. Motion

Very little:

- A value that changed with the last epoch flashes: a 22% ruby background
  that fades over 1.2 s. Off under `prefers-reduced-motion`.
- Charts move to their new values (`blend` in `charts.rs`).
- A slow selection dims the dashboard, and says `Updating…`, only after
  0.15 s, so quick answers do not flicker.
- No hover lifts, no parallax, no entrance animations.

The motion that exists is data that changed.

---

## 7. Voice in the interface

Short, factual, sentence case. Real strings from the app:

- `Click a value or a bar to select it; Ctrl/⌘-click adds to the selection.`
- `Live epoch 812`
- `answered in 17 ms`
- `1 of 6 · revenue`

---

## 8. Doing and not doing

**Do**

- Slate surfaces with 1px edges, teal for the accent, ruby only for a
  selection.
- The system's sans, and mono for the figures around the data.
- Real screenshots, large enough to read, in the theme of the page.
- A live replica of the filter states in the hero, built from the UI's own
  rules (`Landing.vue`).
- Tabular numbers.

**Do not**

- Gradients, glows, glassmorphism. Shadows on anything that does not pop
  over the page. (The replica and the screenshots on the landing page have
  `--shadow-pop`: they stand for a window of the app.)
- A second accent hue. Ruby for anything other than a selection.
- Web fonts, or the old faces (Bricolage Grotesque, Atkinson Hyperlegible).
- Rounded 12 to 16px cards.
- Title Case headings, or "Tidepool" with a capital.
- Sea illustrations, waves (other than the mark's), stock photos, 3D renders.

---

## 9. Assets in this folder

`screenshots/`: 1600×1000 CSS px, 2× device scale, from a copy of
`examples/retail` with `tidepool feed` running, with reduced motion (so no
value is caught mid-flash). Each in `-light.png` and `-dark.png`.

| file | what it shows |
| --- | --- |
| `01-dashboard` | the sales dashboard with no selection, live |
| `02-dashboard-selected` | the same, with `east` selected in the region filter: a ruby chip and bar, every visual filtered. Used on the landing page (`website/public/screens/`, as JPEG). |
| `03-model` | the model browser: the relationship diagram |
| `04-status` | the status page |

Best hero candidate: `02-dashboard-selected`. It shows the ruby selection, and
every visual filtered by it. Missing: the config page with a plan, and the
history with a diff. They need an edit in progress to look like anything.
