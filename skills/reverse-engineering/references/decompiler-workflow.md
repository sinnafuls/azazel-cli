# Decompiler workflow

For a disassembler or decompiler reached through an MCP server (IDA, Ghidra, Binary Ninja and similar expose comparable operations). Tool names differ per server; find the ones for each step with `tool_search` or `/mcp`. The loop is the same everywhere.

## Before you start

1. Open the target and wait for auto-analysis to finish. Results taken while analysis is running are incomplete: missing functions, missing references.
2. Get the overview the server offers: segments, entry points, counts of functions and strings, imports grouped by library.
3. Apply what is free: library signatures, type libraries for the platform, debug information if a symbol file exists. Every function identified as library code is one you never read.

## The loop

Repeat for each function on the path to your answer:

1. **Decompile** the function. Read the pseudocode for shape first: what it takes, what it returns, what it calls, where it loops.
2. **Name what you recognise.** Callees with known imports, constants, string references. Rename local variables that hold recognisable things (`len`, `fd`, `key`).
3. **Type the arguments and locals.** A pointer whose fields are read at fixed offsets is a structure; a value compared against a small set of constants is an enumeration. Setting a type usually rewrites the pseudocode into something readable, and the change propagates to callers.
4. **Name the function** for what it does, with a prefix for the subsystem (`cfg_load`, `net_send_frame`). If you only partly understand it, say so in the name (`maybe_verify_sig`) and fix it later.
5. **Comment** the one thing a reader would not see: the meaning of a magic constant, the invariant, the format being parsed.
6. **Follow references** outward: callers to learn how results are used, callees to resolve unknowns, data references to find siblings that handle the same structure.
7. **Log** the address, new name and evidence in the findings file.

Batch reads. Ask for several functions, or for a function together with its callees, in one call when the server allows it. Use the server's search for strings, byte patterns and immediates rather than scanning listings.

## Recovering structures

1. Find the allocation or the constructor: the allocation size is the structure size.
2. Collect every offset accessed through a pointer of that type, across all functions that receive it. The server's cross-references to structure fields help once a first version exists.
3. Create the structure with the fields you can justify. Name unknown fields by offset (`field_18`) and keep gaps explicit.
4. Apply the type to arguments and globals. Read the functions again: the pseudocode will now show field names, and wrong guesses become visible as nonsense.
5. Refine. Arrays show up as scaled indexing, nested structures as offsets that repeat with a constant stride, tagged unions as a switch on one field followed by different interpretations of another.

C++ specifics: an object whose first field is a pointer to a table of function pointers has a vtable. Name the table after the class (run-time type information gives the real name when present), name its slots as methods, and type `this` in each method.

## Reading faster

- **Switch statements and jump tables** are dispatchers. Name the cases: command handlers, message types, opcode implementations.
- **Error paths** carry strings. A function that logs `"failed to parse header"` parses a header.
- **Paired functions**: where there is `alloc`, find `free`; where there is `encode`, find `decode`. The simpler of the pair explains the other.
- **Constants**: look up unfamiliar 32-bit and 64-bit constants. CRC polynomials, hash initialisation values, S-box prefixes and curve parameters identify algorithms without reading them.
- **Calling convention mistakes** by the decompiler show as arguments that appear from nowhere or vanish. Fix the prototype instead of reading around it.
- **Inlined library code** (string operations, memcpy loops, 64-bit division helpers) is recognised by shape. Comment it and move on.

## When the decompiler output is wrong

- Undefined values or huge stack offsets: the function's bounds or stack frame are wrong. Check the disassembly, redefine the function, correct the stack variables.
- Code that falls through into data: a no-return call was not marked. Mark it.
- Indirect calls with no target: resolve through the vtable or table you typed, or note the candidates from data references.
- Opaque predicates and flattened control flow: note the obfuscation, find the dispatcher variable, and consider whether a trace from the user would be cheaper than untangling it statically.

## Saving the work

Persist the database after each stretch of renaming and typing. If the server supports exporting pseudocode, types and names, export them at the end and list the paths in the report, so a later session or another analyst starts from your state.
