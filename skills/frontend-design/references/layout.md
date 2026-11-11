# Layout

## Spacing scale

Base unit 4 px. Tokens and their typical jobs:

| Token | px | Use |
|---|---|---|
| `space-1` | 4 | Icon to label, tight inline gaps |
| `space-2` | 8 | Between related controls, inside small components |
| `space-3` | 12 | Padding of compact controls |
| `space-4` | 16 | Default padding of cards and list rows; gap between form fields |
| `space-6` | 24 | Between groups inside a section |
| `space-8` | 32 | Between sections of a panel |
| `space-12` | 48 | Between page sections |
| `space-16` | 64 | Between major page sections on wide screens |
| `space-24` | 96 | Hero and landing section rhythm |

Rules of use:

- The space inside a group is smaller than the space around it. If a card has 16 px padding, the gap between cards is 16 px or more, and the gap between a heading and its card group is larger again.
- Pick padding by component size, not by habit: small controls 8 by 12, default 12 by 16, large 16 by 24.
- When two values look almost right, choose the larger. Cramped layouts are far more common than airy ones.

## Page structures

**Application shell.** A fixed sidebar of 220 to 280 px, a top bar of 48 to 64 px, and a scrolling content area with a maximum width. Below about 900 px the sidebar becomes a drawer.

```css
.shell { display: grid; grid-template-columns: 260px minmax(0, 1fr); min-height: 100dvh; }
.content { max-width: 1200px; padding: 24px clamp(16px, 4vw, 40px); }
```

**Content page.** One reading column of at most 65ch, centred, with optional side notes on wide screens.

**Card grid.** Let the grid decide the column count:

```css
.grid { display: grid; gap: 16px; grid-template-columns: repeat(auto-fill, minmax(260px, 1fr)); }
```

**Form.** One column. Labels above fields. Group related fields under a short heading. The primary button sits at the start of the reading direction, directly under the last field. Field width hints at the expected length of the answer.

**Dashboard.** Lead with the two or three numbers that matter, then the chart that explains them, then the table that lists the detail. Do not give every metric the same size.

## Alignment and edges

- Count the vertical edges content aligns to. More than three or four per screen reads as disorder.
- Align text baselines across a row, not box centres, when rows mix text sizes.
- Optical alignment beats mathematical alignment for icons and for text next to rounded shapes; nudge by a pixel when it looks wrong.
- Give the page consistent outer gutters: 16 px on phones, 24 to 40 px on larger screens.

## Separation, weakest to strongest

1. A hairline border.
2. A change of background tone.
3. A shadow (use for things that float: menus, dialogs, dragged items).
4. Space.

Use one method per boundary. Nested boxes with borders inside boxes with borders are a sign the grouping should be done with space instead.

## Radius and elevation

Radius scales with size: 4 px for inputs and small controls, 6 to 8 px for buttons and cards, 12 to 16 px for dialogs and large surfaces, fully round only for pills and avatars. A nested element's radius equals the parent's radius minus the padding between them. Use two or three shadow levels, each with low opacity and a larger blur for higher elevation.

## Robustness

- `min-width: 0` on flex and grid children that contain text, so they can shrink and truncate.
- `overflow-wrap: anywhere` on user-generated strings.
- Reserve space for images and embeds with `aspect-ratio` to avoid layout shift.
- Use logical properties (`padding-inline`, `margin-block`) so right-to-left locales work.
- Use `100dvh`, not `100vh`, for full-height layouts on mobile.
