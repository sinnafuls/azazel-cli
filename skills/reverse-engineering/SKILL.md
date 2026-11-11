---
name: reverse-engineering
description: >-
  Use when the task is to understand software or data without its source: triaging an unknown binary, recovering behaviour and data structures in a disassembler, working out a file format or a network protocol, or finding what changed between two builds. For software you own or are authorised to analyse.
---
# Reverse engineering

The work is hypothesis and proof. You guess what a thing is, find the cheapest observation that would contradict the guess, and write down what survived. Keep a findings file from the first minute; analysis that is not written down is redone.

## Ground rules

- Confirm scope first: what the target is, who owns it, and what question the user wants answered. Analyse only what the user is entitled to analyse. Do not defeat licensing or copy protection, and do not run untrusted samples on the user's machine; if execution is needed, say so and let the user provide an isolated environment.
- State the question as one sentence ("how does the client compute the request signature", "what is the layout of the save file"). Stop when it is answered; everything else is a distraction.
- Separate what you observed from what you infer. Mark inferences as such in notes and in the report.
- Work on a copy. Record the hash of every artefact you analyse.

## Workflow

### 1. Triage (minutes, not hours)

Identify the container, architecture, toolchain and any packing before opening a disassembler. The commands and what to look for are in [triage](references/triage.md). Output of triage: a paragraph that says what the file is, how it was built, what it links against, and where to look first.

### 2. Choose the route

| Question | Route |
|---|---|
| What does this function or feature do | Static analysis, starting from strings and imports |
| What does this file contain | [File-format analysis](references/file-formats.md) |
| What do these two programs say to each other | [Protocol analysis](references/protocols.md) |
| What changed between versions | Version diffing (below) |
| Values only exist at run time | Ask the user for a trace, a dump or a debugger session; then go static from the addresses observed |

### 3. Static analysis

Use the disassembler through its MCP server when one is connected (tools appear as `mcp__<server>__…`; `/mcp` lists them). The loop of renaming, typing and following references is described in [decompiler workflow](references/decompiler-workflow.md). Without a disassembler server, fall back to command-line tools through `shell` (`objdump`, `readelf`, `dumpbin`, `otool`, `strings`, `nm`) and say that depth is limited.

Work from anchors inward, never from the entry point forward:

1. **Strings** that name the feature: error messages, format strings, file names, URLs, registry keys.
2. **Imports** that the feature must use: file, socket, crypto, compression APIs.
3. **Constants**: magic numbers, table sizes, cryptographic constants.
4. Follow cross-references from an anchor to the function that uses it, name that function, then walk to its callers until you reach something you can already explain.

### 4. Record as you go

After every function you understand: rename it, type its arguments, add a one-line comment, and add a line to the findings file with the address and the evidence. Names are how progress is saved; an unnamed function will be analysed again.

### 5. Verify

A claim is verified when something independent agrees with it: a second code path that reads the field you typed, a parser you wrote that round-trips real samples, a recomputed checksum that matches, a captured message that decodes. Prefer writing a small script (a parser, a decoder, a re-implementation of one function) over more reading; `shell` runs it.

## Version diffing

To find what a patch changed:

1. Triage both builds; confirm they are the same product and architecture.
2. Match functions by name where symbols exist, then by structure (size, call targets, constants, string references). A disassembler server's similarity or comparison tools do this; otherwise compare lists of functions with their sizes and referenced strings.
3. Set aside functions that differ only by addresses or register allocation.
4. For each real difference, read both versions side by side and describe the change in terms of behaviour: a new check, a changed bound, a different constant, a removed call.
5. Report changed functions most significant first, with addresses in both builds.

## Writing up

Put the result in a Markdown file in the project (ask where if unclear). Structure:

```
# <target>: <question>

## Answer
<the answer in a few sentences, with confidence: confirmed / likely / open>

## Target
File, size, SHA-256, format, architecture, compiler or packer, version strings.

## Findings
For each: the claim, the evidence (address, function name, bytes, sample), and how it was verified.

## Structures and formats
Tables of offset, size, type, name, meaning. Mark fields whose meaning is a guess.

## How to reproduce
Tools and versions, scripts written (paths), commands run.

## Open questions
What is unknown, and what observation would settle it.
```

Keep scripts beside the report. A reader should be able to rerun the verification without redoing the analysis.

## Habits that save time

- Read the data before the code: a format's structure is often visible in a hex dump in minutes.
- Library code is not your problem. Identify it (signatures, strings, known constants) and skip it.
- When a function is large, find what it returns and who checks that, before reading its body.
- If you have been in one function for a long time without a new name or type to show for it, step back to its callers or to a different anchor.
- Compare with a known-good sample whenever you can: two inputs that differ in one property expose the bytes that encode it.
