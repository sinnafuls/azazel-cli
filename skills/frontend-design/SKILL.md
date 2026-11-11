---
name: frontend-design
description: Use when building or restyling a user interface (a page, a component, a whole app) and the result has to look designed rather than generated. Covers hierarchy, type, spacing, colour, states, responsive behaviour and motion, and how to check the work in a real browser.
---
# Frontend design

Interfaces fail visually for a small number of reasons: nothing is more important than anything else, spacing is arbitrary, colour is decoration instead of information, and states other than the happy one were never drawn. Work through the sections in order; each one removes one of those failures.

## 1. Decide before you style

Write down, in the task notes or the plan, three things before touching CSS:

1. **Who looks at this and what do they do first.** One primary action per screen. If you cannot name it, the layout cannot express it.
2. **The register.** Dense tool, calm reading surface, or expressive marketing page. The register fixes density, type size and how much colour is allowed. Do not mix registers on one screen.
3. **What already exists.** Read the project's tokens, theme file and two or three existing components. Extend that system. A second visual language inside one product is the most common defect.

## 2. Hierarchy

- Build at most three levels of emphasis per view: one primary element, a few secondary, everything else tertiary. Emphasis comes from size, weight, contrast and space around the element, in that order of strength.
- De-emphasise instead of emphasising. Making secondary text lighter and smaller is usually better than making the heading bigger.
- Labels are quieter than the values they label. Numbers a user came to read are the loudest thing in their row.
- Check with a blur: shrink or blur the screenshot. The primary action and the page's subject must still be findable.

## 3. Type

Use one family for interface text, optionally one more for display or code. Pick sizes from a scale, never ad hoc. Body text sits at 14 to 16 px for tools and 16 to 18 px for reading. Line height falls as size rises: about 1.5 for body, 1.2 for headings, 1.0 to 1.1 for display sizes. Keep reading measure between 50 and 75 characters. Details and a ready scale are in [typography](references/typography.md).

## 4. Space and layout

Use a spacing scale built on a base unit of 4 px, and only values from it. Space communicates grouping: related items sit closer than unrelated ones, and the gap between groups is clearly larger than the gaps within them. Align to a small number of vertical edges. Prefer fewer, larger regions to many boxes; a border is the weakest way to separate two things, and space is the strongest. Grid recipes and page structures are in [layout](references/layout.md).

## 5. Colour

Start in greyscale. Add one accent colour for the primary action and for selection. Reserve green, amber and red for status, and never use them as decoration. Neutrals carry most of the interface: a background, a raised surface, a border, and three text strengths. Every text colour must meet contrast against the surface it sits on: 4.5:1 for body text, 3:1 for large text and for the boundaries of controls. Dark themes are designed, not inverted. See [color](references/color.md).

## 6. Components and their states

A component is not finished until every state exists:

| State | What to check |
|---|---|
| Default | Reads as interactive without hovering |
| Hover | A change that is visible but smaller than the pressed change |
| Focus | A ring that is visible on every background, including around dark buttons |
| Active or pressed | Immediate feedback, within one frame |
| Disabled | Lower contrast and no pointer events, with the reason available nearby |
| Loading | The control keeps its size; a skeleton or spinner replaces content, not layout |
| Empty | Says what belongs here and how to add the first item |
| Error | Says what went wrong and what to do, next to the thing that failed |
| Overflow | Long names, long numbers, 0 items, 1 item, 1000 items |

Touch targets are at least 44 px in their smaller dimension on touch layouts. Controls of one kind have one height across the product.

## 7. Responsive behaviour

Design the narrow layout first, then let it widen. Use content-driven breakpoints: add one where the layout breaks, not at device names. Columns collapse in reading order. Tables either scroll inside their own container or turn into stacked records; they never make the page scroll sideways. Test at 360, 768, 1280 and 1920 px wide, and once at 200 % zoom.

## 8. Motion

Motion explains a change of state; it is not decoration. Keep durations between 120 and 240 ms for interface transitions, ease out for things entering and ease in for things leaving. Animate `transform` and `opacity` only. Honour `prefers-reduced-motion` by removing movement and keeping the state change. One animated element at a time.

## 9. Patterns that mark an interface as generated

Remove these unless the brief asks for them:

- A purple-to-blue gradient hero with a centred headline and two buttons.
- Every section as a row of three cards with an icon, a title and two lines of text.
- Emoji used as icons, or icons from three different sets.
- Shadows on everything, or borders and shadows together on the same element.
- Uniform 16 px rounding on elements of very different sizes.
- Glass blur behind text.
- Centred body paragraphs longer than two lines.
- Placeholder copy ("Lorem ipsum", "Feature one", "John Doe") left in a finished screen.
- A dark theme made of pure black and pure white.

## 10. Verify in a browser

Do not judge a layout from its source. Run it and look.

1. Start the dev server as a background `shell` job.
2. `browser` `open` the page, then `screenshot` it. Take one more with `full_page` for long pages.
3. `browser` `resize` to a narrow viewport (360×800) and capture again; repeat at a tablet width.
4. Exercise states: `hover` and `click` the primary controls, tab through with `key`, submit an empty form, and capture each state that matters.
5. Read `console` for errors and `network` for failed requests.
6. Compare what you see with the decisions from step 1 and with the [review checklist](references/review-checklist.md). Fix, then capture again. Finish only when a fresh screenshot shows the fix.

Report what you changed, attach the screenshot paths, and list anything you could not verify.
