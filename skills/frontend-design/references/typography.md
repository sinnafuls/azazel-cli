# Typography

## Choosing faces

- One sans-serif family with at least four weights covers almost every product interface. Add a second family only for a clear job: a serif or display face for headlines, a monospace face for code and tabular figures.
- Prefer a variable font or a system stack over many static files. A system stack (`system-ui, -apple-system, "Segoe UI", Roboto, sans-serif`) costs nothing to load and is the right default for tools.
- Load at most two families and four files. Use `font-display: swap` and preload the one file the first screen needs.

## A scale that works

Sizes in px, with line height. Use these names as tokens.

| Token | Size | Line height | Use |
|---|---|---|---|
| `text-xs` | 12 | 16 | Captions, table meta, badges |
| `text-sm` | 14 | 20 | Dense interface text, labels |
| `text-base` | 16 | 24 | Body |
| `text-lg` | 18 | 28 | Lead paragraphs |
| `text-xl` | 20 | 28 | Card and section titles |
| `text-2xl` | 24 | 32 | Page titles in tools |
| `text-3xl` | 30 | 36 | Page titles on content pages |
| `text-4xl` | 36 | 40 | Marketing headings |
| `text-5xl` | 48 | 52 | Hero, wide screens only |

For fluid headings use `clamp()` between two steps of the scale, for example `clamp(1.875rem, 1.2rem + 2.5vw, 3rem)`. Never scale body text with the viewport.

## Weight and contrast

- Regular (400) for body, medium (500) for labels and buttons, semibold (600) for headings. Bold (700) is rarely needed when size already separates levels.
- Create secondary text with a lighter colour, not a lighter weight. Weights under 400 lose legibility at small sizes.
- All caps only for very short labels, with letter spacing of about 0.04 to 0.08 em and a size one step down.

## Details that show care

- Tighten letter spacing slightly on large headings (about -0.01 to -0.02 em); leave body text alone.
- Use tabular figures (`font-variant-numeric: tabular-nums`) in tables, timers and anything that updates in place.
- Use real quotation marks, apostrophes and ellipses in copy. Use a non-breaking space between a number and its unit.
- Left-align text. Centre only short headings and single lines. Never justify text on the web.
- Limit a text column to `max-width: 65ch`.
- Truncate with an ellipsis only where the full text is reachable (a tooltip, a detail view). Wrap everything else.

## Common faults

| Fault | Fix |
|---|---|
| Headings and body the same visual weight | Drop body contrast one step or raise the heading one scale step |
| Five or more sizes on one screen | Collapse to three; let weight and colour do the rest |
| Long lines on wide screens | Constrain the measure, do not enlarge the font |
| Loose line height on large headings | Reduce toward 1.1 as size increases |
| Grey text on a tinted background failing contrast | Measure it; darken the text or lighten the surface |
