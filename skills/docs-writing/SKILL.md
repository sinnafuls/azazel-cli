---
name: docs-writing
description: >-
  Use when writing or restructuring documentation: a README, a getting-started tutorial, a how-to guide, reference pages or a changelog. Gives the structure for each kind and the checks that keep documentation true.
---
# Writing documentation

Documentation is read by someone who wants to get something done and will stop reading the moment they can. Decide who that is and what they are trying to do, then write only that.

## 1. Pick the kind

Four kinds answer four different needs. Mixing them is the usual reason documentation is hard to use.

| Kind | Reader's state | Shape |
|---|---|---|
| Tutorial | New, wants to succeed at something once | A guided path with one outcome, every step shown |
| How-to guide | Knows the basics, has a specific goal | Numbered steps to that goal, no teaching |
| Reference | Working, needs a fact | Complete, uniform, organised like the thing it describes |
| Explanation | Wants to understand why | Prose about design, trade-offs, background |

A README is a short mix on purpose: what it is, a quick start, and links to the rest.

## 2. Before writing

- Read what exists: the current docs, the code's public surface, the tests and examples. Documentation that contradicts the code is worse than none.
- Run the thing. Every command and snippet you write must have been executed by you in this session, or be marked as not verified.
- Match the project's existing voice, heading style, and file layout.

## 3. README

In this order, and stop when the reader has enough:

1. **Name and one sentence**: what it is and who it is for. No slogans.
2. **What it does**: three to six bullet points of capabilities, or a short example of it in use.
3. **Install**: the shortest working command per supported platform, with prerequisites and versions.
4. **Quick start**: the smallest complete example that produces a visible result, copy-pasteable, with the expected output.
5. **Usage**: the few most common tasks, each a heading with a snippet. Link to the full reference rather than reproducing it.
6. **Configuration**: where the config lives, the options people actually change, defaults.
7. **Development**: how to build, test and run locally.
8. **Status and support**: maturity, where to ask, how to report bugs.
9. **Licence**.

Keep it under a few screens. Move anything long to `docs/` and link it.

## 4. Tutorial

- State what the reader will have at the end and how long it takes.
- List prerequisites with exact versions and how to check them.
- One path, no choices. Every step is an action followed by what the reader should see.
- Show complete files or exact edits, never "add something like".
- Explain only what is needed to continue; link to explanations.
- End with the working result, a way to verify it, and two or three next steps.
- Test it from a clean environment, following your own words literally.

## 5. How-to guide

- The title is the goal: "Rotate the signing key", "Deploy behind a reverse proxy".
- Say when to use it and what must already be in place.
- Numbered steps, each one action. Commands in code blocks, with placeholders clearly marked (`<project-id>`).
- Include the check that shows it worked and the common failures with their fixes.
- No background. Link to it.

## 6. Reference

- Generated from the source where the ecosystem supports it (doc comments, OpenAPI, `--help`), so it cannot drift.
- Uniform entries. For a function or endpoint: signature, one-sentence purpose, parameters (name, type, default, meaning, constraints), return value, errors, one example, version introduced.
- For a CLI: synopsis, each flag with its default and environment variable, exit codes, examples.
- For configuration: every key, type, default, effect, and an example file.
- Complete and dry. No narrative, no opinions.
- Ordered the way the reader searches: alphabetical or by module, with a table of contents.

## 7. Explanation

- One topic per page: the architecture, the data model, why a decision was made.
- Start from the problem, then the approach, then the trade-offs and what was rejected.
- A diagram when there are more than three interacting parts. Keep it as text (Mermaid or similar) so it can be edited and diffed.
- Record decisions with their date and context, so readers know whether they still hold.

## 8. Changelog and release notes

- Written for users: what changed for them, grouped as Added, Changed, Deprecated, Removed, Fixed, Security.
- Newest first, each release with its version and date.
- Breaking changes first, each with the migration.
- No commit subjects pasted in; rewrite them.

## 9. Sentences

- Plain, human prose: apply the `writing-style` skill to anything you write, and again on a final read.
- Second person and active voice: "Run the migration", not "The migration should be run".
- One idea per sentence. Short paragraphs. The important word early.
- Present tense. No "simply", "just", "easily", "obviously": if it were, the reader would not be here.
- The same thing has the same name everywhere. Define a term once, where it first appears.
- Headings say what the section contains; a reader should be able to navigate by headings alone.
- Code, commands, file names and keys in code formatting. Every code block names its language.
- Lists for parallel items, tables for items with attributes, prose for reasoning.
- State versions and dates for anything that changes.

## 10. Examples

- Realistic, minimal and complete: they run as shown.
- Show the output or result.
- Use obviously fake values for secrets and reserved names for hosts and addresses (`example.com`, `192.0.2.1`).
- Prefer examples that are tested: doc tests, or snippets included from files that the test suite runs.

## 11. Verify before finishing

1. Run every command and snippet in a clean checkout or container. Fix or mark what fails.
2. Follow every link, including anchors and relative paths. `grep` the docs for the old names of anything renamed.
3. Check claims against the code: defaults, flag names, limits, error text.
4. Read it as the target reader: is anything assumed that they would not know? Is anything explained that they do not need?
5. Build the documentation site if there is one and look at the rendered page; open it with `browser` and take a screenshot when layout matters.
6. Report what you wrote, what you verified by running it, and what you could not verify.

## Faults

| Fault | Fix |
|---|---|
| The README explains the architecture before the install command | Reorder; move explanation to its own page |
| A tutorial that offers options at every step | Choose one path; mention alternatives at the end |
| Reference written as prose | Make entries uniform; use tables |
| Screenshots of text | Use text |
| "See the code for details" | Write the detail or link to the exact file and line |
| Documentation of what the code used to do | Regenerate or re-verify; delete what cannot be confirmed |
