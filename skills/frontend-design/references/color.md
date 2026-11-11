# Colour

## Roles, not swatches

Name colours by the job they do. Components refer to roles; themes assign values to roles.

| Role | Job |
|---|---|
| `bg` | Page background |
| `surface` | Cards, panels, inputs |
| `surface-raised` | Menus, dialogs, popovers |
| `border` | Hairlines and control outlines |
| `text` | Primary text |
| `text-muted` | Secondary text, labels |
| `text-subtle` | Placeholders, disabled text |
| `accent` | Primary action, selection, focus ring, links |
| `accent-fg` | Text and icons on top of `accent` |
| `success`, `warning`, `danger`, `info` | Status only; each with a quiet background variant |

## Building the neutral ramp

- Use 9 to 11 steps from near-white to near-black. Tint them slightly toward the accent hue (2 to 6 % chroma); pure grey next to a saturated accent looks dirty.
- Steps are perceptually even. Work in OKLCH or LCH: fix hue and chroma, vary lightness.
- Light theme: background at the lightest step, surfaces one step off or pure white, borders around the third step, muted text around the seventh, primary text at the darkest or second darkest.
- Dark theme: background around 12 to 16 % lightness, not black. Raised surfaces get lighter, not shadowed. Reduce chroma of accents by a little so they do not vibrate.

## The accent

One accent hue. Derive a ramp for it the same way as the neutrals and use:

- the mid step for filled buttons and selected states,
- a darker step for hover and pressed,
- a very light step for selected rows and subtle highlights,
- a step that reaches 4.5:1 against the surface for link text.

A second accent is justified only by a second meaning, such as a distinct product area.

## Status colours

Status is never conveyed by colour alone. Pair it with an icon, a label or a position. Use the quiet variant (tinted background, darker text of the same hue) for banners and badges, and the strong variant only for destructive buttons and critical alerts.

## Contrast

| Pair | Minimum ratio |
|---|---|
| Body text on its surface | 4.5:1 |
| Text of 24 px or more, or 19 px bold | 3:1 |
| Icons that carry meaning, control borders, focus rings | 3:1 against what they touch |
| Disabled text | No requirement, but it must still be readable |

Measure every text and surface pair, including muted text on tinted surfaces, placeholder text, and white text on the accent. Measuring in the browser: `browser` `eval` with `getComputedStyle(el).color` and the background, then compute the ratio; or take a screenshot and inspect it.

## Data colours

For charts, use a categorical set whose members differ in lightness as well as hue, so they survive greyscale and colour-vision differences. Limit a chart to six categories; group the rest as "other". Use a single-hue ramp for ordered data and a two-hue diverging ramp only when the data has a meaningful midpoint.

## Faults

| Fault | Fix |
|---|---|
| Colour everywhere, nothing stands out | Return to greyscale; add the accent to one thing per view |
| Accent used for both actions and decoration | Decoration becomes neutral |
| Pure black text on pure white | Soften text to the second-darkest neutral |
| Dark theme by inversion | Rebuild from the role table with a dark ramp |
| Red and green as the only difference between two states | Add an icon or label |
