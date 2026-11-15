# Contributing to azazel

Issues and pull requests are both welcome. Issues are how bugs and ideas reach the project: they are read by people and by
the agents that work on azazel, so a clear issue gets fixed sooner.

## Ground rules

- **Search first.** Look through [open and closed issues](https://github.com/sinnafuls/azazel-cli/issues?q=is%3Aissue)
  and the [docs](docs/README.md); add to an existing issue instead of opening a second.
- **One problem or idea per issue.** Two bugs in one issue mean one of them gets lost.
- **Never paste secrets.** No API keys, tokens, passwords, cookies or contents of `~/.azazel/auth.json`. Check logs,
  `azazel doctor` output and config for them before you paste.
- **Security problems are private.** Report them through
  [a private advisory](https://github.com/sinnafuls/azazel-cli/security/advisories/new), not as an issue.
- **Be kind and concrete.** Say what you did, what you expected and what happened.

## Reporting a bug

Use the **Bug report** template. What helps most:

| | How to get it |
|---|---|
| Version | `azazel --version`; try `azazel update` first, it may be fixed |
| System | OS, terminal, and tmux/screen/zellij or ssh if you use them |
| Steps | The shortest way to make it happen again, and whether it happens every time |
| Screenshot | Anything drawn wrong: a screenshot is worth more than a description |
| Diagnosis | `azazel doctor`, run in the same project |
| Log | The end of `~/.azazel/logs/azazel.log`; start azazel with `--debug` for more |

## Asking for a feature

Use the **Feature request** template. Start with the problem you have, not only the solution you picture; if another
tool does it well, say which and how.

## Pull requests

1. Open an issue first for anything bigger than a small fix, so the approach can be agreed before you write it.
2. Read [AGENTS.md](AGENTS.md): it describes the layout, the conventions and how to try a change with the scripted
   provider, without a real model or your own credentials.
3. Keep a pull request to one change. Add a test for what a user can see, update the page under [docs/](docs/README.md)
   that owns the behaviour, and add a line under `## [Unreleased]` in [CHANGELOG.md](CHANGELOG.md).
4. Run the checks the pull request template lists. CI runs the whole test suite on Linux for every pull request.

Contributions are licensed under the project's licences, [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT).
