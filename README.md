# azazel

A team of coding agents in your terminal. One fast Rust binary runs a lead agent that can hand work to
specialists running in parallel (scouts, a planner, coders, testers, a reviewer, a verifier and more), with
the model of your choice for each, and checks the build before it calls a job done.

```text
› add rate limiting to the API and make sure the tests pass
```

## What it does

- **Works as a team.** Specialists run side by side, each in its own context and optionally its own git
  worktree. Message or stop any of them while they work.
- **Any model, local or cloud.** Anthropic, OpenAI, Google, OpenRouter, Groq, DeepSeek, xAI, Mistral, Ollama,
  LM Studio, llama.cpp, Bedrock, Vertex and Azure, plus Claude and ChatGPT subscription sign-in. Every role
  takes a fallback chain.
- **Goals with a checker.** Give it an objective; it keeps going until the project's build and tests pass and
  an independent verifier agrees.
- **Safe by default.** Permission modes from read-only planning to full auto, a checkpoint before every
  prompt (`/rewind`), and agents never `git push`.
- **Sees and acts.** Web search and fetch, a real Chromium browser, screen capture and input for playtesting
  apps and games, language-server diagnostics after every edit.
- **Remembers.** Per-project memory and a searchable code index that carry across sessions.
- **Makes assets.** Images, video, 3D models and audio, and sprite sheets packed for Godot, Unity, Roblox,
  Unreal or plain JSON.
- **Extensible.** MCP servers, hooks on any tool call, Luau extensions, custom agents, skills, slash commands,
  packages, and an embeddable Rust API.

## Install

Linux and macOS:

```sh
curl -fsSL https://raw.githubusercontent.com/sinnafuls/azazel-cli/main/install/install.sh | sh
```

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/sinnafuls/azazel-cli/main/install/install.ps1 | iex
```

Then open a new terminal, `cd` into a project and run `azazel`. The first start walks you through signing in
or adding an API key. Update later with `azazel update`.

From source (Rust 1.99): clone the repository and run `./install/install.sh` or `.\install\install.ps1`.

## Use

```sh
azazel                               # interactive UI in the current project
azazel -p "explain src/main.rs"      # one prompt, answer on stdout
azazel goal "make cargo test pass"   # work until the checker verifies it
azazel bg "fix the flaky test"       # in the background; azazel attach <id> opens it later
azazel schedule add every 1h "triage new issues"   # on a clock, or --on ci-failure / file:<glob>
azazel --ssh build-box               # the same UI on a session that runs on another machine
azazel doctor                        # what is configured and what is missing
```

## Documentation

Start with [getting started](docs/getting-started.md); every feature has its own page in [docs/](docs/README.md).
To work on azazel itself, read [AGENTS.md](AGENTS.md).

## Licence

Licensed under either of [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your option.
