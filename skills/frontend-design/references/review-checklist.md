# Review checklist

Run this against screenshots, not source. Mark each line pass, fail or not applicable, and fix the fails before reporting.

## First look (five seconds)

- [ ] The subject of the page is obvious.
- [ ] One action is clearly primary.
- [ ] Nothing looks broken: no overlap, clipping, unstyled element or stray scrollbar.

## Hierarchy and type

- [ ] At most three levels of emphasis.
- [ ] At most three or four text sizes in view, all from the scale.
- [ ] Labels are quieter than values.
- [ ] Reading columns are no wider than about 75 characters.
- [ ] No orphaned single words on heading lines at common widths.

## Space and alignment

- [ ] Every gap is a value from the spacing scale.
- [ ] Gaps inside groups are smaller than gaps between groups.
- [ ] Content aligns to few vertical edges.
- [ ] Outer gutters are consistent across pages.
- [ ] No box-in-box-in-box nesting where space would do.

## Colour

- [ ] The accent appears on the primary action and selection, not on decoration.
- [ ] Status colours mean status.
- [ ] Body text meets 4.5:1; large text and control boundaries meet 3:1.
- [ ] The dark theme, if any, was checked separately.

## Components

- [ ] Hover, focus, pressed, disabled, loading, empty and error states exist for what the page uses.
- [ ] The focus ring is visible on every control.
- [ ] Controls of the same kind share a height and radius.
- [ ] Icons come from one set and one stroke weight.
- [ ] Long content was tried: long names, large numbers, many items, no items.

## Responsive

- [ ] 360 px: no horizontal page scroll, targets at least 44 px, text not shrunk below 14 px.
- [ ] 768 px and 1280 px: columns reflow sensibly; nothing stretches to absurd widths.
- [ ] 200 % zoom still works.

## Motion

- [ ] Transitions are between 120 and 240 ms and explain a state change.
- [ ] `prefers-reduced-motion` is honoured.

## Content

- [ ] No placeholder copy or sample names remain.
- [ ] Buttons name their action ("Save changes", not "OK").
- [ ] Error messages say what happened and what to do.
- [ ] Dates, numbers and currencies are formatted for the locale.

## Runtime

- [ ] The console has no errors.
- [ ] No failed network requests.
- [ ] Nothing shifts as fonts and images load.

## Report

List what failed and was fixed, what failed and remains with the reason, and the screenshot paths that show the final state.
