# tidepool: visual language

Everything here is taken from the running product (`frontend/style.css`,
`frontend/src/charts.rs` and the screenshots in `screenshots/`). It describes
what tidepool looks like today, so the site can look like the same thing.

---

## 1. The one-sentence brief

tidepool looks like **a calm, readable dashboard**: cool paper, deep teal ink,
and one coral that means "selected" and nothing else. The filter lists carry
the design. Everything else is quiet.

If the site reads as "the page a team keeps open all day", it is right. If it
reads as a SaaS hero with gradients and floating cards, it is wrong.

The site is not kayak's site with other colours. kayak is dark, dense and
grey, with mono everywhere. tidepool is light and dark, with room, and a
display face.

---

## 2. Palette

The CSS custom properties of `frontend/style.css`. Use them verbatim.

| token | light | dark | used for |
| --- | --- | --- | --- |
| `--paper` | `#eef3f2` | `#0f2321` | the page |
| `--surface` | `#f8fbfa` | `#142c2a` | visuals, cards, inputs, code |
| `--ink` | `#14302f` | `#e2eeeb` | text |
| `--ink-soft` | `#4d6664` | `#9db5b1` | labels, secondary text, axes |
| `--tide` | `#1f7a73` | `#4fb3a9` | the brand colour: links, the top rule of a visual, a possible value, the first series |
| `--kelp` | `#c9d8d4` | `#2b4441` | an excluded value, table rules |
| `--sand` | `#b9c7c3` | `#35504c` | borders, splits, gridlines |
| `--anemone` | `#d9466f` | `#f07a98` | **selected**, and only selected (chips, a selected value, a brush, a value that just changed) |
| `--problem` | `#a23b2c` | `#f0937f` | errors |

Rules:

- **Coral means selected.** The UI has two other uses: the pulse dot when
  paused, and the `n updates waiting` badge. Do not add more. On the site,
  coral appears only where a replica shows a selection.
- **Teal is the brand.** Links, the active nav entry, the top rule of a card.
- **No gradients, no shadows.** Surfaces are flat. A card is `--surface` on
  `--paper` with a 3px `--tide` rule on top.
- Both themes are first-class. The UI follows `prefers-color-scheme`. The site
  has the VitePress toggle and the same two palettes.

### The three filter states

The most characteristic picture of tidepool. A value in a filter list is a
row, 28px high, with a 4px bar on the left:

| state | bar | fill | text |
| --- | --- | --- | --- |
| selected | `--anemone` | `--anemone` | white |
| possible | `--tide` | `--surface` | `--ink` |
| excluded | `--kelp` | none | `--ink-soft` |

A tree adds a twist (▸/▾) and indents by depth. A node inside a selected
branch gets a 16% wash of `--anemone`. A node with a selection below it gets a
7px coral dot.

---

## 3. Typography

Three families:

- **Bricolage Grotesque** (500, 700), `--display`: the brand, page titles,
  visual titles, KPI values, card titles. Letter-spacing `-0.01em` at large
  sizes.
- **Atkinson Hyperlegible** (400, 700), `--text`: all other text. Base size
  15px, line height 1.45.
- **`ui-monospace, "SF Mono", Menlo, monospace`**, `--mono`: code, file
  names, YAML, `curl`. 13px.

Numbers are tabular (`font-variant-numeric: tabular-nums`) everywhere.

Sizes in the UI: brand 20px, page titles 24 to 26px, section titles 15 to
18px, body 15px, small 12 to 13px. A KPI value is up to 44px. The site needs
larger display sizes for the hero. Use Bricolage Grotesque 700 for them.

**Lowercase.** The brand is `tidepool`. Headings on the site are lowercase, by
the copy style. The UI itself uses sentence case for titles ("Sales
overview"), because they are the user's names.

---

## 4. Geometry and spacing

- **`border-radius: 4px`** on inputs and buttons. Chips and badges are pills
  (`999px`). Cards and visuals have no radius.
- **The card**: `--surface`, `border-top: 3px solid var(--tide)`, padding
  14px 16px. No border on the other sides. On hover (a link card) the rule
  turns `--anemone`.
- **Code blocks**: `--surface`, `border-left: 3px solid var(--tide)`.
- **A notice**: `border-left: 3px solid var(--anemone)`.
- **The dashboard grid**: 12 columns, rows of 80px, gaps of 16px. A filter
  rail of 230px on the left.
- Page padding 24px (16px on a phone). Gaps of 14 to 24px. Room, but not
  airy.

---

## 5. The shapes that make up the UI

**The strip** (the top bar): the brand in Bricolage 20px, the nav in
`--ink-soft` with a 2px `--tide` underline on the current entry, and at the
right the pulse: an 8px dot (`--tide` when live, `--sand` offline, `--anemone`
when paused), the epoch, and pause/resume.

**The toolbar** of a dashboard: its name in Bricolage 24px, then the selection
as coral chips (`customers.region` `North` ×), back and forward.

**The rail**: filter lists, trees and date ranges. Each with a title in
Bricolage 15px and a search box.

**Visuals**: KPI (a large number and the change against a comparison, teal up,
coral down), bar (horizontal past six bars; a selected bar coral, the rest
teal), line (teal area under the first series; a brush in coral), table
(sortable, rules in `--kelp`).

**Charts** come from `charts.rs`: axis lines in `--sand`, labels in
`--ink-soft`, split lines in `--sand` at 40% opacity, bars at most 28px wide.

**The config page**: files on the left (mono 13px, the current one with a
teal bar), the editor in the middle (mono, a teal bar on the left), the plan
on the right with class badges: `safe` (teal outline), `rebuild` (`--kelp`
fill), `destructive` (coral fill).

**The relationship diagram** (model page): rectangles in `--surface` with a
1.5px `--tide` stroke, names in Bricolage, edges in `--ink-soft` with an arrow
to the one side.

---

## 6. Motion

Very little:

- A value that changed with the last epoch flashes: a 30% coral background
  that fades over 1.2 s. Off under `prefers-reduced-motion`.
- ECharts animates a bar to its new length.
- No hover lifts, no parallax, no entrance animations.

The motion that exists is data that changed.

---

## 7. Voice in the interface

Short, factual, sentence case. Real strings from the app:

- `Click a value or a bar to select it; Ctrl/⌘-click adds to the selection.`
- `Live · epoch 812 · 20 rows/s`
- `Waiting for approval`
- `an agent: …` (in the history)

---

## 8. Doing and not doing

**Do**

- Paper and surface, teal rules, coral only for a selection.
- Bricolage Grotesque for display, Atkinson Hyperlegible for text.
- Real screenshots, large enough to read, in the theme of the page.
- A live replica of the filter states in the hero.
- Tabular numbers.

**Do not**

- Gradients, glows, glassmorphism, drop shadows.
- A second accent hue. Coral for anything other than a selection.
- Rounded 12 to 16px cards.
- Title Case headings, or "Tidepool" with a capital.
- Sea illustrations, waves, stock photos, 3D renders.

---

## 9. Assets in this folder

`screenshots/`: 1600×1000 CSS px, 2× device scale, from `examples/retail`
with `tidepool feed` running. Each in `-light.png` and `-dark.png`.

| file | what it shows |
| --- | --- |
| `01-dashboard` | the sales dashboard with no selection, live |
| `02-dashboard-selected` | the same, with a region selected: a coral chip and bar, every visual filtered. Used on the landing page (`website/public/screens/`, as JPEG). |
| `03-model` | the model browser: the relationship diagram |
| `04-status` | the status page |

Best hero candidate: `02-dashboard-selected`. It shows the coral selection, and
every visual filtered by it. Missing: the config page with a plan, and the
history with a diff. They need an edit in progress to look like anything.
