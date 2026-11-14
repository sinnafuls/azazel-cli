---
description: Error handling in Rust
globs: ["crates/**/*.rs"]
alwaysApply: false
condition: "\\.unwrap\\(\\)"
interruptMode: stop
---
Return `Result`; never `unwrap()` outside tests.
