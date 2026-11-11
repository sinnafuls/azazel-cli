---
name: design-tokens
description: >-
  Use when starting a project's visual system or replacing scattered values with one: derives colour, type, spacing, radius and shadow tokens from a brief or an existing brand colour and emits them as CSS variables and a Tailwind theme.
---
# Design tokens

A token is a named decision. Components use names; themes map names to values. The goal is that changing the brand colour, the density or the theme is a change in one file.

## 1. Gather inputs

Find or ask for:

- One brand or accent colour (a hex value), or a description to choose one from.
- The register: dense tool, standard application, or spacious content.
- Whether a dark theme is needed.
- What already exists: a Tailwind config, a CSS variables file, a component library theme. Read it first; extend and rename rather than add a parallel set.

## 2. Three layers

1. **Primitive tokens**: the raw ramps. `neutral-50` to `neutral-950`, `accent-50` to `accent-950`, status ramps, the type scale, the spacing scale. No component uses these directly.
2. **Semantic tokens**: roles that point at primitives. `bg`, `surface`, `border`, `text`, `text-muted`, `accent`, `accent-fg`, `danger`. Themes differ only here.
3. **Component tokens**, only where needed: `button-height`, `input-radius`. Add one when the same semantic choice repeats in several components and may change together.

## 3. Derive the colour ramps

Work in OKLCH so steps are perceptually even.

1. Convert the accent to OKLCH and keep its hue `h`.
2. Accent ramp: lightness from 0.97 down to 0.20 in 11 steps (50, 100, 200 … 900, 950). Chroma peaks around the 500 to 600 steps and falls toward both ends so light steps are not neon and dark steps are not muddy.
3. Neutral ramp: the same lightness steps, hue `h`, chroma 0.005 to 0.02.
4. Status ramps: fixed hues (green about 150, amber about 80, red about 25, blue about 250), built like the accent ramp. If the accent is close to a status hue, shift that status hue by 15 to 20 degrees so they stay distinct.
5. Check contrast for the pairs the semantic layer will create, and adjust the lightness of single steps rather than the whole ramp.

## 4. Type, space, radius, shadow

| Group | Tokens | Rule |
|---|---|---|
| Font families | `font-sans`, `font-mono`, optional `font-display` | At most two loaded families |
| Type scale | `text-xs` … `text-5xl`, each with a line height | Ratio about 1.2 for tools, 1.25 for content; round to whole px |
| Weights | `font-normal` 400, `font-medium` 500, `font-semibold` 600 | Three weights are enough |
| Spacing | `space-1` … `space-24` | 4 px base: 4, 8, 12, 16, 20, 24, 32, 40, 48, 64, 96 |
| Radius | `radius-sm` 4, `radius-md` 8, `radius-lg` 12, `radius-full` | Dense tools go down one step, playful brands up one |
| Shadow | `shadow-sm`, `shadow-md`, `shadow-lg` | Low opacity, neutral-tinted, larger blur with elevation |
| Motion | `duration-fast` 120 ms, `duration-base` 180 ms, `ease-out`, `ease-in` | |
| Layers | `z-dropdown`, `z-sticky`, `z-overlay`, `z-modal`, `z-toast` | Named, never raw numbers in components |

Density: for a dense register reduce control heights (28, 32, 36 px) and default padding by one spacing step; for a spacious register raise them by one.

## 5. Emit CSS variables

Primitives and semantics in one file, themes as overrides of the semantic layer only.

```css
:root {
  /* primitives */
  --neutral-50: oklch(0.985 0.004 264);
  --neutral-900: oklch(0.21 0.012 264);
  --accent-500: oklch(0.62 0.19 264);
  --accent-600: oklch(0.55 0.2 264);
  --space-4: 1rem;
  --radius-md: 0.5rem;

  /* semantic */
  --bg: var(--neutral-50);
  --surface: #fff;
  --border: var(--neutral-200);
  --text: var(--neutral-900);
  --text-muted: var(--neutral-600);
  --accent: var(--accent-600);
  --accent-fg: #fff;
  color-scheme: light;
}

[data-theme="dark"] {
  --bg: var(--neutral-950);
  --surface: var(--neutral-900);
  --border: var(--neutral-800);
  --text: var(--neutral-100);
  --text-muted: var(--neutral-400);
  --accent: var(--accent-400);
  --accent-fg: var(--neutral-950);
  color-scheme: dark;
}
```

Write every step of every ramp; the excerpt shows the shape. Provide hex fallbacks in a comment or a generated second file when the project must support browsers without OKLCH.

## 6. Emit the Tailwind theme

Point Tailwind at the variables so utilities and plain CSS share one source.

Tailwind 4 (CSS-first):

```css
@import "tailwindcss";
@theme {
  --color-bg: var(--bg);
  --color-surface: var(--surface);
  --color-border: var(--border);
  --color-text: var(--text);
  --color-muted: var(--text-muted);
  --color-accent: var(--accent);
  --radius-md: var(--radius-md);
  --font-sans: "Inter", system-ui, sans-serif;
}
```

Tailwind 3 (`tailwind.config`): put the same mapping under `theme.extend.colors`, `borderRadius`, `fontFamily` and `spacing`, each value a `var(--…)` reference. Check which major version the project uses in its `package.json` before writing either form.

## 7. Apply and verify

1. Replace literal values in existing components with tokens, one component at a time. Search for hex colours, raw px values and `rgb(` to find them.
2. Build, then open the app with `browser` and take screenshots of the main screens in each theme.
3. Confirm contrast for text on `bg`, `surface` and `accent`, and that focus rings are visible.
4. Record the tokens and their intended use in a short `DESIGN.md` or the project's existing design notes, so later work uses names instead of new values.

## Faults to avoid

- Naming tokens after values (`blue-button`, `gray-text`): they become lies when the theme changes.
- Components reading primitives directly, which makes theming impossible.
- Dozens of one-off component tokens. Add them on the second use, not the first.
- A dark theme that only swaps background and text and leaves borders, shadows and accents untouched.
