---
name: ui-review
description: >-
  Use to audit an existing interface for accessibility and usability problems: contrast, keyboard and focus order, semantics and names, forms and error states, responsive behaviour. Produces a findings report with severity, evidence and a fix for each item.
---
# UI review

An audit, not a redesign. Find what blocks or slows users, prove each finding with evidence from the running interface, and say how to fix it. Do not change code unless the task asks for fixes as well.

## Method

1. **Scope.** List the pages and flows under review. If none are named, take the entry page, the main task flow end to end, and one form.
2. **Run it.** Start the app as a background `shell` job and `browser` `open` each page. Never review from source alone; source review comes second, to locate causes.
3. **Walk each flow four ways**: by sight (screenshots), by keyboard only, by structure (the accessibility snapshot), and at a narrow width.
4. **Record findings as you go**, each with the evidence that shows it.
5. **Rank and report.**

## Checks

### Structure and names (from `browser` `snapshot`)

The snapshot is what assistive technology receives. Read it as a user who cannot see the page.

- Every interactive element has a role and a name that says what it does. `button ""`, `link "click here"` and three links all named "Read more" are findings.
- One `main` region, one level-one heading, headings that descend without skipping levels.
- Images that carry information have text alternatives; decorative ones are hidden from the tree.
- Form fields are named by a visible label, not only by a placeholder.
- Custom controls (menus, tabs, dialogs, comboboxes) expose the right role and state (`expanded`, `selected`, `checked`).
- Status messages that appear without a page load are announced (`role="status"` or `alert`).

### Keyboard (with `browser` `key`)

- `Tab` reaches every interactive element in visual order and nothing else.
- The focused element is always visibly indicated. Take a screenshot after each few stops.
- `Enter` and `Space` activate buttons; arrow keys move within menus, tabs and radio groups; `Escape` closes overlays.
- Opening a dialog moves focus into it, focus stays inside while it is open, and closing returns focus to the control that opened it.
- No keyboard trap: you can always tab away.
- A skip link or landmark lets users pass repeated navigation.

### Contrast and colour

- Body text at least 4.5:1, large text and control boundaries at least 3:1. Measure with `eval`: read `getComputedStyle` for the element's colour and the effective background, then compute the ratio.
- Information is not carried by colour alone (required fields, errors, chart series, link text inside paragraphs).
- Check both themes when there are two.

### Forms and errors

- Submit the form empty and with invalid values.
- Each error appears next to its field, in text, and is tied to the field (`aria-describedby` or equivalent) so it is announced.
- Focus moves to the first invalid field or to an error summary.
- Errors say what is wrong and how to fix it. "Invalid input" is a finding.
- Input is preserved after a failed submit.
- Fields use the right `type` and `autocomplete` values; required fields are marked in text.
- Destructive actions ask for confirmation or offer undo.

### Responsive and zoom

- At 360 px wide: no horizontal page scroll, targets at least 44 px, no clipped text.
- At 200 % zoom: content reflows, nothing overlaps.
- Content is not lost when orientation or width changes.

### Motion and timing

- Animations stop or simplify under `prefers-reduced-motion`.
- Nothing flashes more than three times a second.
- Timeouts warn before expiring and can be extended.

### Usability

- The primary action of each screen is obvious and reachable without scrolling on common sizes.
- System state is visible: loading, saving, saved, failed.
- Empty states explain what belongs there and how to start.
- Labels use the user's words, and the same thing has the same name everywhere.
- The `console` shows no errors and `network` no failed requests during the flows.

## Severity

| Level | Meaning |
|---|---|
| `blocker` | Some users cannot complete the task at all (unreachable control, keyboard trap, unreadable text, unlabeled required field) |
| `major` | The task can be completed only with difficulty or guesswork |
| `minor` | A friction or inconsistency with an easy workaround |
| `note` | An improvement that is not a defect |

## Report format

```
# UI review: <scope>

Summary: <n> blockers, <n> major, <n> minor. <one sentence on the overall state>

## Findings

### 1. <short title> (blocker)
Where: <page or URL>, <element as it appears in the snapshot, e.g. button "" [ref=e14]>
Evidence: <screenshot path, or the snapshot lines, or measured ratio 2.8:1>
Impact: <who is affected and how>
Fix: <the concrete change, with file and line when you located the cause>
Criterion: <WCAG 2.2 success criterion when one applies, e.g. 1.4.3 Contrast (Minimum)>

### 2. …

## Not checked
<flows or conditions you could not exercise, and why>
```

Order findings by severity, then by how early in the flow they occur. Report only what you observed; mark anything inferred from source without running it as unverified.
