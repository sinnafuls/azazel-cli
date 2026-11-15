## What and why

<!-- What changes for a user, and why. Link the issue: "Fixes #123". -->

## How it was checked

<!-- The tests you added or ran, and anything you tried by hand (with the scripted provider, see AGENTS.md). Screenshots for anything on screen. -->

## Checklist

- [ ] `cargo fmt --all`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test -p <each crate this touches>`
- [ ] `cargo build --no-default-features -p azazel-cli`
- [ ] `cargo xtask docs-check`
- [ ] The page under `docs/` that owns the behaviour is updated
- [ ] A line under `## [Unreleased]` in `CHANGELOG.md`, written for a user
- [ ] No secrets, keys or personal paths in the diff
