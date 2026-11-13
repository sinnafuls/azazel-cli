---
description: Review a branch against another
argument-hint: <branch> [base]
---
Review $@ carefully. Compare $1 with ${2:-main}.
Then look at the rest: ${@:2}
