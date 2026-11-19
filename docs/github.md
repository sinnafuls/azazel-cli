# GitHub

Azazel reads and writes GitHub through the [`gh`](https://cli.github.com) program, signed in with `gh auth login`. Every feature on this page exists only when `gh` is on `PATH`; without it the commands say so and point to <https://cli.github.com>. `azazel doctor` reports whether `gh` was found.

Reading counts as network access to `github.com`, like `pr://` and `issue://` always did (see [permissions](permissions.md)). Anything that **posts** to GitHub (opening a pull request, posting a review) asks first in every [mode](permissions.md) except `bypass`, and is refused in headless runs, which have no one to ask.

## Reading an issue or a pull request

| Address | Returns |
|---|---|
| `issue://<n>` | Title, state, author, labels, description and comments |
| `pr://<n>` | The same, plus the branch and its base, whether it is a draft, the review decision, a summary of the checks (`3 passed, 1 failed, 0 pending (lint)`) and the changed files with their line counts |
| `pr://<n>/diff` | The pull request's diff |

`issue://` and `pr://` alone list what is open. The agent reads these with the `read` tool; see [URL schemes](tools.md#url-schemes-read-accepts).

## `#12` in a prompt

A `#<n>` in a prompt (a whole word of digits, such as `look at #12`) attaches that issue or pull request when the prompt is sent, the way `@file` attaches a file. Azazel asks `gh` for the issue and then the pull request of that number in the repository of the working directory. A number that does not resolve, or any `#` that is not a whole number, stays plain text. At most five numbers of one prompt are looked up.

## `/issues` and `/prs`

Popup lists of the repository's issues or pull requests: number, title, author, labels and age.

| Key | Does |
|---|---|
| `Tab` | Steps through open, closed and all |
| `Enter` | Puts `#<n>` in the input |
| `o` | Opens the row in your browser |
| `Esc` | Closes the list |

## `/pr [title]`

Opens a pull request from the current branch.

1. It refuses, saying what to do, on the repository's default branch, with a detached `HEAD`, with uncommitted changes, or when the branch has no commits the default branch lacks.
2. The lead's small model drafts a title and description from `git log <base>..HEAD` and the diff. Text after `/pr` is used as the title. The request carries the same few lines of style rules as `/commit`.
3. You edit the draft in a form: the first line is the title, the rest the description. An empty text cancels.
4. It asks whether to push the branch to `origin` and open the pull request, and shows what will be sent. Nothing is pushed before you say yes (in `bypass` mode the question is skipped).
5. It pushes the branch, runs `gh pr create` and prints the pull request's address.

## `/review [n]`

Reviews pull request `n`, or the one of the current branch, with the `reviewer` [role](models-and-logins.md) and the `code-review` skill. The request carries the pull request as `pr://<n>` reads it and its diff, and nothing is run. The findings are shown in the transcript as `[P0]` to `[P3]` lines (P0 a blocker, P1 a major defect, P2 minor, P3 a nit). Afterwards it asks whether to post them with `gh pr review --comment`. It never approves and never requests changes.
