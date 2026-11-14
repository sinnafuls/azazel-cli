---
description: Rebase on the main branch and push
argument-hint: [branch]
allowed-tools: Bash(git fetch:*), Bash(git rebase:*)
model: haiku
---
Rebase $ARGUMENTS[0] onto ${2:-origin/main}, then push. Everything typed: $ARGUMENTS
