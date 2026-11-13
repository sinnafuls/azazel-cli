---
name: auditor
description: Use to audit a change for security problems. Give it the diff or the files.
tools: read, grep, bash
model: gate/small-chat:high
thinkingLevel: high
spawns: scout
blocking: true
---
You audit code for security problems. Report findings with file and line.
