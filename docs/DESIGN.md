# sheprd design language

One idea: **industrial and quiet**. Precise type and hairlines carry the structure. There are no ornaments, no boxes
inside boxes, and no decoration that competes with what the agents are saying.

All tokens live at the top of the design-language block in `web/style.css` (`--f-*`, `--t-*`, `--r-*`, `--ease`). The
shared treatments (section header, name, secondary line) are single selector lists in style.css. Feature files keep only
layout (margins, gaps), so the look changes in one place.

## Type: three levels, three faces

| Level | Face | Used for |
|---|---|---|
| Identity | **Michroma** (`--f-mark`, `fonts/wordmark.woff2`, caps-only subset A–Z 0–9 `- . _ :`) | The home agent's wordmark and the host name in the Agents bar. Nothing else. |
| Mid | **Rajdhani** 500/600 (`--f-mid`, `fonts/rajdhani-{500,600}.woff2`) | Section headers, names (agents, repos, apps, sheets), secondary lines, buttons, tab labels, short labels and badges, the Home briefing line, the Decide question. |
| Reading | **System** (`--f-body`: SF Pro on the phone) | Chat message bodies, markdown, previews of what someone said, notes and explanations longer than one line, form inputs. |
| Code | **Mono** (`--mono`) | Code blocks, commands, paths, branch names, terminal. Never used for labels or status. |

Rajdhani has a small x-height (0.51 em vs SF's 0.53 at a narrower width), so it is always set about 1–2 px larger than the
system text it replaces.

### Scale (px, at 375–402 pt widths)

| Token | Spec | Where |
|---|---|---|
| `--t-display` | Rajdhani 600 26/1.12 | Home briefing line, Decide question |
| `--t-title` | Rajdhani 600 23/1.15 | Sheet titles (`.sheet h2`) |
| `--t-name` | Rajdhani 600 17.5/1.2 | Agent / row / card names, chat header name |
| `--t-name-sm` | Rajdhani 600 16/1.2 | Names in dense lists (grid cells, sheet rows, popup menu rows) |
| `--t-sub` | Rajdhani 500 14.5/1.25 | Secondary lines: job line, status · time, meta, toast sub-line |
| `--t-small` | Rajdhani 500 13/1.2 | The smallest label: tab bar, tiny counts, column heads |
| `--t-sec` | Rajdhani 600 13.5/1.2, caps, tracking .2em | Section headers only |
| body | system 15/1.4 (markdown 15/1.45) | reading text |

## Section headers

Tracked Rajdhani caps after a 5 px accent **diamond** (a square turned 45°), with a **hairline** (`--sep`, 0.5 px) running to
the right edge or to the header's trailing action (Edit, count, caret, "+1 more"). The colour is fg2, the count is fg3.
This is the only section-header style (Agents groups, You should see this, sheets' h3, Settings, History, Machine, Repos,
Tidy/Catch-up groups, Decide, Undo). Caps are allowed **only** here. A header is a line, never a notch in a box.

## Names and secondary lines

- Names: Rajdhani 600, in the agent's pastel text shade (`--pt`) when it has one, otherwise fg.
- Secondary line: Rajdhani 500, **sentence case**, fg2. Layout = job line under the name, dim time on the right of the
  name row (`.since`, `.tm`). Status words are plain words ("working", "asleep · 2h"); colour only where it is the status
  (work blue, done green text, attention dot).
- **No small mono ALL-CAPS sublines** ("3H AGO", "WORKING", "RECAP", "YOU", "LIVE", "W1:P1 · 7 PROCS"). Inline tags become
  Rajdhani 600 in sentence case, without a box.

## Numerals

- Static numbers in a line (counts, "3h", "5 changed") take the line's face (Rajdhani).
- Rajdhani has no tabular figures, so **numbers that tick or stand in columns** (Machine live stats and columns,
  countdowns, gauges, the usage tank, context rings) use system `tabular-nums`. That avoids jitter. Mono is never used
  for a number unless it is part of code.
- Markdown tables (md.js `table()`): no boxes, hairline (`--sep`) rows, a slightly stronger hairline under the header,
  header in Rajdhani 600 fg2. Columns marked right or centre by the alignment colons take `tabular-nums` when numeric or
  short; short columns stay on one line; long prose columns get an 8 em min width so they wrap into lines. Cells wrap at
  words (the chat's break-anywhere does not reach them). A table wider than the message scrolls sideways in its own box.

## Surfaces, frames, radius

Frames are low-key and HUD-like:
- **Near-square corners**: `--r-card` 3, `--r-ctl` 3, `--r-btn` 2. Sheets keep 8 at the top (`--r-sheet`). Real
  circles stay circles: avatars, rings, dots, the send button. Chat bubbles and the composer field keep their soft shape
  because they are reading surfaces.
- Cards: `--bg2` with `--card-sh` in light (a very soft shadow) and surface colour alone in dark. Never a coloured edge,
  a dashed box or a tinted wash.
- **Hero surfaces** get a square-cut panel with a **hairline frame** (`.hud-frame`, fg at 22%, replacing the shadow) and
  **accent corner brackets** just outside it (`.hud-brackets`: 10 px arms, 1.5 px, 4 px out). There are only three: the
  Agents grid, the Decide entry and the Home hero card. The brackets are the only accent on a surface. Everything else
  stays calm, with its shadow or nothing.
- **Primary buttons** have a clipped top-right corner (`.hud-cut`, `--r-cut` 7 px). Other buttons are square-ish.
- Hairlines (`--sep`, 0.5 px) separate rows inside a card and run after headers. Prefer a hairline to a new box. The
  Scheduled and Running apps strips are sections (header and hairline rows), not dashed boxes.
- Marks are flat. The home agent is the accent diamond, usage is the tank. No glow, no glimmer, no scan effects beyond
  the one working bar under the wordmark.

## Colour

- One accent: iOS system blue (`--accent`; `--accent-fill` under white text). Links, the primary button, the selected tab,
  the diamond.
- Status hues are dots and meters only: `--work` blue, `--done` green, `--attn` amber dot, `--warn` amber for meters,
  `--red` for errors and destructive actions. **No green "online" dot when all is well**: a connection dot shows only when
  something is wrong.
- Agent identity = the 7 pastels (fill `--pf`, text `--pt`), on names and avatars, never as an edge.
- Light and dark both hold AA for secondary text (fg2) at the sizes above.

## Spacing

4 pt grid. Screen gutter 12 (lists) / 16 (sheets, reading). Rows ≥ 44 pt tall. Header-to-content gap 6, section gap 14–18.

## Motion

`--ease` = cubic-bezier(.2, .8, .2, 1). 150 ms for presses and fades, 260 ms for sheets, folds and view slides. Content
updates morph in place (no blank re-render, no reflow flash). Lists fold from their header caret. Every animation
respects `prefers-reduced-motion`.

## Left alone on purpose

Chat message bodies and markdown (system font, readable), code blocks / commands / paths (mono), form inputs (system,
≥ 16 px so iOS never zooms), the terminal, app thumbnails, avatar discs and the tank.
