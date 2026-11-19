# Models and logins

Azazel talks to any number of model providers, local or cloud. You name models with a short spec, group them into roles with fallback chains, and give each provider a credential: an environment variable, a stored API key, a subscription sign-in, or the cloud platform's own identity. No model id is built in; you choose one with `azazel init`, `/settings`, or `--model`.

## Model specs

| Form | Meaning | Example |
|---|---|---|
| `provider/model` | One model of one provider | `anthropic/claude-sonnet-4-5` |
| `provider/model:level` | The same, at a fixed [thinking level](#thinking-levels) | `openai/gpt-5:high` |
| `@role` | Whatever the role's chain resolves to | `@smol` |

The provider is everything before the first `/`. Model ids may contain `/` and `:` themselves (`openrouter/anthropic/claude-haiku-4.5`); a trailing `:suffix` is read as a thinking level only when it names one.

A list of specs is a fallback chain: the first model that answers is used.

## Roles

Roles are named chains in `[roles]`. Agents, hooks and internal jobs refer to roles, so you can re-point a whole class of work in one place.

```toml
# ~/.azazel/config.toml
[roles]
default = ["anthropic/claude-sonnet-4-5", "openai/gpt-5"]
smol    = ["ollama/<local-model>", "@default"]
slow    = ["anthropic/claude-opus-4-5:high", "@default"]
```

| Role | Built-in value | Used by |
|---|---|---|
| `default` | none: you must set it | Every role below, until it is given a chain of its own |
| `smol` | `@default` | Compaction summaries, `prompt` hooks, MCP sampling |
| `slow` | `@default` | Hard work: reviewing, planning |
| `plan` | `@slow` | Planning |
| `verify` | `@default` | The checker (see [goals](goals.md)) |
| `memory` | `@smol` | Memory extraction (see [memory](memory.md)) |
| `vision` | `@default` | Work that reads images |
| `judge` | `@smol` | Quick judgements: `classify` in `run_code` scripts and extensions (see [tools](tools.md#code-execution)) |
| `lead` | `@default` | The lead agent |
| `scout` | `@smol` | The `scout` agent |
| `planner` | `@plan` | The `planner` agent |
| `coder`, `tester`, `researcher`, `designer`, `artist` | `@default` | The agent of the same name |
| `reviewer` | `@slow` | The `reviewer` agent |
| `verifier` | `@verify` | The `verifier` agent |
| `playtester` | `@vision` | The `playtester` agent |

The lead starts on `@lead`, and each built-in agent on the role named after it, so any one of them can be given its own model without touching the others: [`/models`](tui.md#models) writes `[roles] <name> = ["provider/model:high", "@slow"]` (the model, its thinking level, its fallbacks), and you can write the same by hand.

You may define further roles and use them anywhere a spec is accepted. When a chain is resolved, roles are expanded in order, a model that already appeared is dropped, and an unknown role or a role that refers back to itself is skipped.

A role's value comes from the last of these that sets it: its built-in value, `~/.azazel/config.toml` (home), the project's `.azazel/config.toml` (project; a trusted project only), `.azazel/config.local.toml` (local), or a change made in the running session. `azazel roles`, the RPC `list_roles` command and `/settings` → Models name that source beside each role. Clearing a role in one file lets the next one down show through, and in the end the built-in value.

A thinking level written on a role's model (`smol = ["openai/gpt-5-mini:low"]`) is that role's own [thinking level](#thinking-levels): the role's requests ask for it unless a level was chosen for the session.

### `azazel roles`

| Command | Does |
|---|---|
| `azazel roles` | Lists every role: its chain, the model it resolves to, that model's thinking level and where the value comes from (`built-in`, `home`, `project`, `local`, or `unset`) |
| `azazel roles set <role> <model> [fallback…] [--thinking LEVEL] [--project]` | Points the role at the model, then the fallbacks in order, in the home config (or the project's with `--project`, which a project that is not trusted refuses). A model is named as `--model` takes it (`@role`, `provider/id`, or part of a name); `--thinking` writes the level on the first model (`provider/id:high`). Comments in the file are kept |
| `azazel roles clear <role> [--project]` | Takes the role out of the home config (or the project's), so it falls back to the next file, or its built-in value; a role of your own goes away |
| `azazel roles preset save <name>` | Saves every role as it is now as `[presets.<name>.roles]` in the home config |
| `azazel roles preset load <name> [--project]` | Writes every role of the preset into the home config (or the project's) |

Every form takes `--json`: the listing prints one JSON object per role (`role`, `chain`, `source`, `builtin`,
`resolved`, `thinking`), a change prints the rows of the roles it changed, and `preset save` prints the preset's name,
roles and file. A mistake in how the command is called (an unknown level, a name that cannot be a role, a project
that is not trusted, a preset that does not exist) exits with status 2; a config file that cannot be written exits
with status 1.

```sh
azazel roles set smol anthropic/claude-haiku-4-5 @default --thinking low
azazel roles preset save cheap
```

### Presets

A preset is a saved set of role chains, for switching the whole team between, say, a cheap and a thorough set of models.

```toml
# ~/.azazel/config.toml
[presets.cheap.roles]
default = ["anthropic/claude-haiku-4-5"]
slow    = ["anthropic/claude-sonnet-4-5"]

[presets.thorough.roles]
default = ["anthropic/claude-opus-4-5:high", "openai/gpt-5"]
```

| Where | Effect |
|---|---|
| `/preset` | Lists the presets and the roles each one sets |
| `/preset <name>` | Points each role of the preset at its models, for this session. Roles the preset does not name stay as they are. When it sets `lead` or `default`, the lead goes back to following its role, so a model picked with `/model` does not outlive the preset |
| `/preset save <name>` | Writes every role as it is now to `[presets.<name>.roles]` in `~/.azazel/config.toml`, replacing a preset of that name |
| `--preset <name>` | Applies the preset to the roles of this run; an unknown name exits with status 2 and lists the presets |

## Choosing the model

| Where | Effect |
|---|---|
| `azazel init` | Asks for a provider and model, writes `roles.default` to `~/.azazel/config.toml` |
| `/settings` → Models → `default` | Same, from inside the TUI; takes effect at once |
| `--model <spec>` | The lead's chain for this run; repeat the flag to add fallbacks. Part of a name is enough |
| `--provider <id>` | Only this provider's models for this run: part of a name given to `--model` or `/model` is looked up among them, `provider/model` of another provider is refused, and the `/model` picker lists only them. Without `--model` the lead keeps the models of its configured chain that are this provider's, and the run ends with status 2 when there are none. Roles that name their own models (`smol`, `verify`, agent definitions) are not changed. A [virtual model](#virtual-models) picks among every provider's models, so it is refused and left out of the picker while `--provider` is given |
| `/models` (or `/model` alone) | [One screen](tui.md#models) for the model of every role, its fallbacks and thinking level, the `Ctrl+P` cycle and presets, with the models of every provider; providers without a credential are signed in to or given a key from there. Changes go to `~/.azazel/config.toml`, or the project's config |
| `Ctrl+L` or `Alt+P` | Picker over roles, favourites, the models chosen lately, [virtual models](#virtual-models) and the models of providers that have a credential: the lead's model for this session |
| `Ctrl+P`, `Ctrl+Shift+P` | The next or previous favourite model or role (`tui.favourites`, or `--models a,b,c` for one run); a step onto a role makes its chain the lead's. See [models and roles at a key](tui.md#models-and-roles-at-a-key) |
| `/model <spec> [fallback …]` | Sets the lead's chain for this session; part of a name is enough |
| `/preset <name>`, `--preset <name>` | Re-points every role a [preset](#presets) names |
| `model:` in an agent definition, `[agents.<name>] model` | Per-agent chain (see [team](team.md)) |

When every `--model` is a concrete model, that chain also becomes this run's `default` role, so the roles and agents built on `@default` follow it.

On the command line, in `/model` and in the RPC `set_model` command a model may be named loosely. Each name is
resolved in this order:

1. `@role` names a role.
2. `provider/id` with a provider azazel knows is taken as written, listed in the catalogue or not (local servers
   name their own models). `virtual/<name>` is a [virtual model](#virtual-models) an extension defines.
3. A model id, among the providers you can use: those with a credential, the local servers you [set up](#which-providers-are-listed) and the cloud platforms (all providers when `--api-key` is given).
4. The single best fuzzy match of `provider/id` among the same models, so `fable` or `haiku:high` work.

When several models match equally well, the run stops with status 2 and names up to five of them. Configuration
files stay strict: they need `provider/id` or `@role`.

`azazel models [QUERY] [--provider ID] [--all] [--json]` lists the models of the providers you have a credential
for, under a heading per provider that says where its credential comes from (`subscription` for a sign-in). Each
row has the model, its context window, output limit, price per million tokens in and out (`subscription` for a
provider you signed in to, `free`), the thinking levels it takes (`yes` when the catalogue names none), whether it
reads images, and the roles that answer with it first. `--all` lists every provider in the catalogue, those without
a credential headed `no credential`. With `--json` each model is one object: `model`, `name`, `context_window`,
`max_output`, `vision`, `reasoning`, `thinking_levels`, `cost`, `roles` (the roles that answer with it), `login`
(used through a sign-in) and `usable` (its provider is set up).

### Which providers are listed

`azazel models`, `/model`, `Ctrl+L` and the model picker of [remote](remote.md) list the models of the providers
that are set up, and leave the others out:

- A provider that needs a key: once it has a credential.
- A local server that needs none (`ollama`, `lmstudio`, `llamacpp`, `litellm`, `vllm`, …): once `[providers.<id>]`
  names it (an empty table is enough), a variable its address or key is read from is set (`LITELLM_BASE_URL`,
  `OLLAMA_API_KEY`), or it listed its models in this session (`/models`, then refresh).
- Bedrock and Vertex: once their credentials can be [found](#bedrock-vertex-and-azure).

A provider that is left out still answers a model named in full (`ollama/qwen3:8b`), and `--all` lists it.

```sh
azazel --model anthropic/claude-sonnet-4-5 --model ollama/<local-model> -p "summarise src/"
```

Without a resolvable model, print mode and the subcommands stop with `no model configured: run "azazel init" or pass --model provider/id`. The interactive UI starts anyway so you can use `/settings`.

## Virtual models

A virtual model is a name you choose like a model that stands for none: for each request made with it, an
[extension](extensions.md) picks the model the request goes to. Use one to route by task, cost or the state
of the conversation, for example quick questions to a small model and hard ones to a large one, while you
stay on one entry of `/model`.

```lua
-- ~/.azazel/extensions/auto_model.luau
azazel.router("auto", function(request)
    -- Requests after tool results, and retries, stay on the model that began the turn.
    if request.reason ~= "user" and request.previous then
        return request.previous
    end
    if string.find(string.lower(request.text or ""), "why", 1, true) then
        return { model = "@slow", thinking = "high" }
    end
    return "@smol"
end)
```

`azazel.router(name, route)` registers `virtual/<name>`. It is listed in `/model` beside the roles, and
`--model virtual/auto`, `/model virtual/auto`, a role (`[roles] default = ["virtual/auto"]`), an agent
definition's `model:` and the RPC `set_model` take it like any other model. `--model`, `/model` and `set_model`
refuse a virtual model that no loaded extension defines, and name the ones there are.

`route(request)` runs before every request and returns where it goes: `"provider/model[:thinking]"`,
`"@role"`, a list of them (a fallback chain, tried in order as any chain is), or `{ model = ..., thinking =
"high" }`, whose level goes to the models named without one. A level chosen with the virtual model
(`virtual/auto:high`) holds for what the router returns without a level. It cannot return a virtual model.

| Field of `request` | Meaning |
|---|---|
| `name` | The virtual model's name, without `virtual/` |
| `reason` | `user`: the first request after something you wrote, steering included. `continuation`: any other request of a turn, such as the one after tool results. `retry`: the request is sent again after an attempt that failed while its reply was arriving. `direct`: a request from outside a conversation, such as a summary or a side question |
| `thinking` | The thinking level the request asks for |
| `previous` | The model that wrote the latest reply of the conversation, as `provider/model` |
| `failed` | For a `retry`: `{ model, error }` of the attempt that failed |
| `text` | The latest message you wrote |
| `messages` | The conversation: `{ role, text, tools }` per message, with the first 2000 characters of its text and the names of the tools an assistant message calls |

Returning `previous` for a `continuation` keeps the provider's prompt cache valid; changing models in the
middle of a turn is allowed and pays for the context once more. The function may wait: it can ask
`azazel.classify` before it answers, which adds that request's time before the first token. It keeps state of
its own in local variables or `azazel.state`.

What you chose and where each request went are kept apart. The session records the choice as a
`model_change` entry (`virtual/auto`), so a resumed session comes back on the virtual model, and each request
as a `request_started` entry with the model it went to and `via`, the virtual model that picked it; replies
name the model that wrote them. The status line shows both, `virtual/auto -> anthropic/claude-sonnet-4-5`;
before the first reply it shows the virtual model alone. The context window, prices and `/cost` are those of
the models that answered.

When the router raises an error, returns nothing or names no model that is configured, the request fails
with `virtual/<name>: <reason>`, unless the chain has a model after the virtual one, which then answers. When
the extension is no longer loaded, a session that was left on its virtual model goes on with the default
chain; the interactive UI says so. `/reload` registers the routers of the scripts that are there then.

## Retry, cooldown and fallback

```toml
[retry]
max_attempts = 3      # attempts per model before moving on
cooldown_secs = 300   # how long a failed model is skipped
```

- Network errors, overload, rate limits and HTTP 5xx are retried on the same model. The delay is the provider's `retry-after` when it sends one, otherwise 1 s doubling per attempt, capped at 60 s, plus up to 250 ms of jitter.
- When the attempts are used up, or at once on an authentication failure, the model goes into cooldown and the next one in the chain is tried. The TUI prints `<from> is unavailable; falling back to <to>`.
- Models in cooldown are skipped unless every model in the chain is.
- A response that breaks off mid-stream is discarded and counts as one attempt.
- Context overflow, an invalid request and a user abort are not retried and do not fall back.
- A used-up usage allowance (a subscription's 5-hour or weekly window, for example) is not retried on the same account. With [several accounts](#several-accounts) the same model is asked again on the next one; otherwise the next model in the chain is tried.

### Stalls

A provider that accepts a request and then sends nothing would otherwise hold a turn for minutes. A watchdog gives such a request up:

```toml
[retry]
first_event_secs = 20   # from sending the request to the first sign of a response
stall_secs = 120        # between two signs of life once the response has begun
max_stalls = 3          # stalls of one model within a request before moving down the chain
```

- Anything the provider sends counts as a sign of life, a keep-alive line too. Response headers alone do not: the first limit runs until the body begins.
- A stalled attempt is dropped whole. What had arrived of the reply is not kept, not stored in the session and never sent back to the model.
- The same model is asked again after a pause (1 s, doubling per stall, capped at 60 s). The transcript and print mode say `no answer from <model> for 20 s; retrying (attempt 1)`.
- Stalls have their own count: they do not use up `max_attempts`. When a model has stalled `max_stalls` times within one request it goes into cooldown and the chain moves on as after any other failure; with one model in the chain the request fails with `no answer for 20 s`.
- `0` turns a limit off: `first_event_secs = 0` and `stall_secs = 0` wait for as long as the connection lives (the HTTP client gives up a connection that is silent for 5 minutes), and `max_stalls = 0` asks the stalled model again without end.
- The limits apply to every HTTP provider, Amazon Bedrock, Vertex AI and providers added by extensions. A provider's model list, asked for once when the catalogue does not know a model, is given up after `first_event_secs` too.
- One provider can have its own limits: `first_event_secs` and `stall_secs` in `[providers.<id>]` replace the `[retry]` ones for it, and `timeout_secs` limits the whole request, from sending it to the end of the reply, however lively the reply is. A request that takes longer fails with `no complete answer within <n> s (the provider's timeout_secs)` and is retried like a network error. `0` turns a limit off; `timeout_secs` is off unless set.

A local server that loads a model on the first request can take longer than 20 seconds to begin; raise `first_event_secs` for it, or set it to `0`:

```toml
[providers.ollama]
first_event_secs = 300   # loading a model takes a while
stall_secs = 0

[providers.gateway]
timeout_secs = 600       # the gateway closes requests after ten minutes anyway
```

## Service tiers

Some providers serve a request faster for a higher price, or slower for a lower one. The tier applies to every agent's requests of the run; summaries and other side requests stay at the provider's default.

| Where | Effect |
|---|---|
| `/fast [on\|off\|status]` | The priority tier: faster output at a higher price. Without an argument it switches between on and off |
| `/slow [on\|off\|status]` | The economy tier: cheaper and slower |
| `--tier priority\|standard\|economy` | The tier at launch; `standard` is the same as naming none |

| Wire API | `priority` | `economy` |
|---|---|---|
| `openai-responses`, `openai-chat`, `openrouter`, `azure-openai-*` | `service_tier: "priority"` | `service_tier: "flex"` |
| `anthropic-messages` (Claude models only) | Fast mode: `speed: "fast"`, with the `fast-mode-2026-02-01` beta header on the models where it is a preview (Claude Opus 5 and the Opus 4 models) | Nothing: Anthropic has no cheaper tier for a live request |
| The others | Nothing | Nothing |

While a tier is on, the status line shows `fast` or `slow` before the thinking level. A model that refuses the tier is asked again without it and stays without it for the rest of the session; the transcript says so once (`<model> does not take the priority tier; …`). The catalogue has no tier prices, so `/cost` and the status line count at standard rates; `/cost` says so while a tier is on.

## Prompt cache

Providers charge less, and answer sooner, for the start of a request that repeats the request before it. What
is cached is a prefix: the tools, then the system prompt, then the conversation. One changed byte early in a
request makes everything after it new, so azazel keeps what changes out of the prefix:

- The system prompt holds only what stays the same for the session: identity, rules, the team, the skill index,
  the environment (OS, shell, directories), the context files, and what MCP servers and extensions add.
- What can change from one request to the next is sent after the conversation, as a `<system-note>` that is
  not kept in the transcript: the date and the git branch, the permission mode (plan mode), the goal, what
  memory keeps in the prompt, and the agent's task list. Switching the mode, setting a goal, saving a memory or
  passing midnight therefore leaves the cache as it was.
- Tools are sent in name order, whatever order they were registered in.
- On the `anthropic-messages` API the cache marks go on the system prompt, the last two user turns, and, once
  the conversation has 32 messages, on a user turn before its middle. That mark moves only every 16 messages,
  so when something near the end changes (a rewind, a retried reply) the older half is still read from the
  cache.
- On a Claude subscription login every mark asks for a 1-hour cache instead of the 5-minute one: the login
  pays out of its usage window, not by the token, and a cache that outlives a pause saves sending the whole
  conversation again.

### The cache clock

The `cache` segment of the [status line](tui.md#status-line) and the web status bar count down the time until the
provider drops the cache (`4:12`, then `cold`). A request that reads the cache starts it again. The lifetime is
what the provider's usage reports when it does: Anthropic says how many tokens were written with the 5-minute
and with the 1-hour lifetime, and the clock takes the longer one that was used. When the usage says nothing it is
5 minutes, or an hour on a Claude subscription login. It counts from when the request was sent, not from when its
answer ended. `get_state` carries it as `cache_left_secs` and `cache_ttl_secs` ([RPC](rpc.md#commands)).

With `[cache] suggest_handoff_at` set (for example `200000`), a context past that size gets one notice per answered
request when a minute is left: `/handoff` and `/compact` read the cache while it is warm, and once it has run out
the next request sends the whole conversation again. The notice makes no request, and nothing is sent differently.

Editing `AGENTS.md`, connecting an MCP server, changing the model or the thinking level, and compaction do
change the prefix; the next request pays for it once.

`/cost` shows how well this works: the share of the session's input that was read from the cache, the same per
model (read, written, not cached, requests), and the last requests that missed the cache. A miss is a request
that read less than the one before it although the conversation had not been cut down. It is listed with its
cause, the first thing in the request that was not what it had been:

| Cause | Meaning |
|---|---|
| the model changed | Each model has a cache of its own |
| the tool list changed | Which tools were added, removed or changed (an MCP server connected, a mode that withholds tools) |
| the system prompt part `<name>` changed, was gained or lost | For example `context_files` after an edit to `AGENTS.md` |
| the thinking level changed | Anthropic keys the conversation's cache on it |
| message N of M is not what was sent before | An old tool result was shortened, or a hook rewrote the conversation |
| N min passed since the request before | The provider keeps an unused cache for 5 minutes (an hour on a Claude subscription login) |
| nothing that was sent changed | The provider dropped it |

`[cache] report_misses = true` also prints the miss in the transcript when it happens.

When the lead's cache runs out while the session waits for you, the UI says so as it happens, in a muted line:

```text
the prompt cache of anthropic/claude-opus-5-5 ran out after 5 min without a request: the next one sends its
84.2k cached tokens anew, about $1.58 instead of $0.13; [cache] warming = "idle" keeps it alive while you think
```

The two amounts are what the cached part of the conversation costs to write to the cache again and what reading
it would have cost; they are left out on a subscription and for a model without a known price. The line is said
once per idle spell, only for a cached prefix of at least 4096 tokens, only while the agent is idle, and only in
a session that runs in this process. With [warming](#keeping-the-cache-warm) on it comes when the warming has
ended and the cache has run out after it; the hint about warming is left out then, and for models whose cache a
request cannot renew.

### Keeping the cache warm

A cache that nothing reads for 5 minutes (an hour on a Claude subscription login) is gone, and the next prompt
pays full price for the whole conversation. `[cache] warming = "idle"` prevents that while you think: 30 seconds before the cache would
expire, azazel sends the lead's last request again asking for a single token, which reads the cache and so
renews it. Reading costs a tenth of the input price; writing the conversation anew costs more than the full
price.

- Only for the lead, only on the `anthropic-messages` API (the Claude subscription and the Anthropic API), and
  only when the cached prefix is at least 4096 tokens.
- It stops `[cache] warm_minutes` (30) after the lead's last own request, when a warming request fails, and
  when the run's cost limit is reached. A prompt of yours takes over at once.
- `/cost` shows the warming requests, what they read and cost (also in the session's spend, as `cache
  warming`), and when the next one is due.

It is off by default: on a subscription each warming request counts towards the usage window.

## Wire APIs

A provider speaks one wire API. `api` in a provider's settings is one of:

| `api` | Requests go to | Spoken by |
|---|---|---|
| `anthropic-messages` | `{base_url}/v1/messages` | Anthropic and endpoints compatible with it |
| `openai-responses` | `{base_url}/responses` | OpenAI and gateways that offer the Responses format |
| `openai-chat` | `{base_url}/chat/completions` | Most hosted and local servers |
| `openrouter` | `{base_url}/chat/completions` | OpenRouter: the chat format plus its `reasoning` object (`minimal` to `xhigh`), a cache breakpoint for `anthropic/…` models, and the price OpenRouter reports for each response |
| `google-generative` | `{base_url}/models/{id}:streamGenerateContent` | The Gemini API |
| `google-gemini-cli` | `{base_url}/v1internal:streamGenerateContent` | Google Cloud Code Assist, reached with a Gemini CLI sign-in |
| `ollama-chat` | `{root}/api/chat` | Ollama's own API (`root` is `base_url` without a trailing `/v1` or `/api`); answers as JSON lines and takes a `think` switch |
| `azure-openai-responses` | `{resource}/openai/v1/responses` | Azure OpenAI; the model is a deployment |
| `azure-openai-chat` | `{resource}/openai/deployments/{deployment}/chat/completions?api-version=…` | Azure OpenAI, deployment-scoped |
| `bedrock-converse` | the AWS SDK | The `bedrock` provider only |

Wire APIs that other harnesses ship and azazel does not: Cursor, Factory Droid, GitLab Duo (agent and proxy), Devin and Google Antigravity are proprietary agent protocols tied to their own clients; Apple Foundation Models is an in-process macOS bridge, not an HTTP API; ChatGPT's Codex transport extras (WebSocket, server-side conversation state) are not needed by the `openai-codex` provider, which uses plain `openai-responses`.

## Built-in providers

| Id | Name | Wire API | Base URL | Key variable |
|---|---|---|---|---|
| `abliteration` | Abliteration | `openai-responses` | `https://api.abliteration.ai/v1` | `ABLITERATION_API_KEY`, `ABLIT_KEY` |
| `aiand` | ai& | `openai-chat` | `https://api.aiand.com/v1` | `AIAND_API_KEY` |
| `aimlapi` | AIML API | `openai-chat` | `https://api.aimlapi.com/v1` | `AIMLAPI_API_KEY` |
| `alibaba-coding-plan` | Alibaba Coding Plan | `openai-chat` | `https://coding-intl.dashscope.aliyuncs.com/v1` | `ALIBABA_CODING_PLAN_API_KEY` |
| `alibaba-token-plan` | Alibaba Token Plan | `openai-chat` | `https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1` | `ALIBABA_TOKEN_PLAN_API_KEY`, `BAILIAN_TOKEN_PLAN_API_KEY` |
| `anthropic` | Anthropic | `anthropic-messages` | `https://api.anthropic.com` | `ANTHROPIC_API_KEY`; sign-in |
| `azure` | Azure OpenAI | `azure-openai-responses` | `${AZURE_OPENAI_BASE_URL:-https://${AZURE_OPENAI_RESOURCE_NAME}.openai.azure.com}` | `AZURE_OPENAI_API_KEY` |
| `baseten` | Baseten | `openai-chat` | `https://inference.baseten.co/v1` | `BASETEN_API_KEY` |
| `bedrock-mantle` | Amazon Bedrock Mantle | `openai-responses` | `https://bedrock-mantle.${AWS_REGION:-us-east-1}.api.aws/v1` | `AWS_BEARER_TOKEN_BEDROCK` |
| `cerebras` | Cerebras | `openai-chat` | `https://api.cerebras.ai/v1` | `CEREBRAS_API_KEY` |
| `charm-hyper` | Charm Hyper | `openai-chat` | `https://hyper.charm.land/v1` | `CHARM_HYPER_API_KEY`, `HYPER_API_KEY` |
| `cline-pass` | ClinePass | `openai-chat` | `https://api.cline.bot/api/v1` | `CLINE_API_KEY` |
| `cloudflare-ai-gateway` | Cloudflare AI Gateway | `openai-chat` | `https://gateway.ai.cloudflare.com/v1/${CLOUDFLARE_ACCOUNT_ID}/${CLOUDFLARE_GATEWAY_ID}/compat` | `CLOUDFLARE_AI_GATEWAY_API_KEY`, `CLOUDFLARE_API_TOKEN` |
| `cloudflare-workers-ai` | Cloudflare Workers AI | `openai-chat` | `https://api.cloudflare.com/client/v4/accounts/${CLOUDFLARE_ACCOUNT_ID}/ai/v1` | `CLOUDFLARE_API_TOKEN`, `CLOUDFLARE_AUTH_TOKEN`, `CLOUDFLARE_API_KEY` |
| `commandcode` | Command Code | `openai-chat` | `https://api.commandcode.ai/provider/v1` | `COMMAND_CODE_API_KEY`, `COMMANDCODE_API_KEY`, `CMD_API_KEY` |
| `coralbricks` | CoralBricks | `openai-chat` | `https://inference.coralbricks.ai/v1` | `CORAL_API_KEY`, `CORALBRICKS_API_KEY` |
| `coreweave` | CoreWeave Serverless Inference | `openai-chat` | `https://api.inference.wandb.ai/v1` | `COREWEAVE_API_KEY`, `WANDB_API_KEY` |
| `deepinfra` | DeepInfra | `openai-chat` | `https://api.deepinfra.com/v1/openai` | `DEEPINFRA_API_KEY` |
| `deepseek` | DeepSeek | `openai-chat` | `https://api.deepseek.com` | `DEEPSEEK_API_KEY` |
| `firepass` | Fire Pass | `openai-chat` | `https://api.fireworks.ai/inference/v1` | `FIREPASS_API_KEY` |
| `fireworks` | Fireworks | `openai-chat` | `https://api.fireworks.ai/inference/v1` | `FIREWORKS_API_KEY` |
| `github-copilot` | GitHub Copilot | `openai-chat` | `https://api.githubcopilot.com` | `COPILOT_GITHUB_TOKEN`; sign-in |
| `gmi-cloud` | GMI Cloud | `openai-chat` | `https://api.gmi-serving.com/v1` | `GMI_API_KEY` |
| `google` | Google Gemini API | `google-generative` | `https://generativelanguage.googleapis.com/v1beta` | `GEMINI_API_KEY`, `GOOGLE_API_KEY`, `GOOGLE_GENERATIVE_AI_API_KEY` |
| `google-gemini-cli` | Google Cloud Code Assist (Gemini CLI) | `google-gemini-cli` | `https://cloudcode-pa.googleapis.com` | none: sign-in only |
| `groq` | Groq | `openai-chat` | `https://api.groq.com/openai/v1` | `GROQ_API_KEY` |
| `helmcode` | Helmcode | `openai-chat` | `https://api.helmcode.com/v1` | `HELMCODE_API_KEY` |
| `huggingface` | Hugging Face | `openai-chat` | `https://router.huggingface.co/v1` | `HUGGINGFACE_HUB_TOKEN`, `HF_TOKEN` |
| `inception` | Inception (Mercury) | `openai-chat` | `https://api.inceptionlabs.ai/v1` | `INCEPTION_API_KEY` |
| `kilo` | Kilo Gateway | `openai-chat` | `https://api.kilo.ai/api/gateway` | `KILO_API_KEY` |
| `kimi-code` | Kimi Code | `openai-chat` | `https://api.kimi.com/coding/v1` | `KIMI_API_KEY` |
| `litellm` | LiteLLM | `openai-chat` | `${LITELLM_BASE_URL:-http://localhost:4000/v1}` | `LITELLM_API_KEY` (optional) |
| `llamacpp` | llama.cpp | `openai-chat` | `http://localhost:8080/v1` | `LLAMACPP_API_KEY`, `LLAMA_CPP_API_KEY` (optional) |
| `lmstudio` | LM Studio | `openai-chat` | `http://localhost:1234/v1` | `LMSTUDIO_API_KEY`, `LM_STUDIO_API_KEY` (optional) |
| `meta` | Meta Model API | `openai-responses` | `https://api.meta.ai/v1` | `MODEL_API_KEY`, `META_API_KEY`, `META_MODEL_API_KEY` |
| `minimax` | MiniMax | `anthropic-messages` | `https://api.minimax.io/anthropic` | `MINIMAX_API_KEY` |
| `minimax-code` | MiniMax Code | `anthropic-messages` | `https://api.minimax.io/anthropic` | `MINIMAX_CODE_API_KEY` |
| `minimax-code-cn` | MiniMax Code CN | `anthropic-messages` | `https://api.minimax.cn/anthropic` | `MINIMAX_CODE_CN_API_KEY` |
| `mistral` | Mistral | `openai-chat` | `https://api.mistral.ai/v1` | `MISTRAL_API_KEY` |
| `modular` | Modular | `openai-chat` | `https://api.modular.com/v1` | `MODULAR_API_KEY` |
| `moonshot` | Moonshot | `openai-chat` | `${MOONSHOT_BASE_URL:-https://api.moonshot.ai/v1}` | `MOONSHOT_API_KEY` |
| `nanogpt` | NanoGPT | `openai-chat` | `https://nano-gpt.com/api/v1` | `NANO_GPT_API_KEY` |
| `novita` | Novita | `openai-chat` | `https://api.novita.ai/openai/v1` | `NOVITA_API_KEY` |
| `nvidia` | NVIDIA NIM | `openai-chat` | `https://integrate.api.nvidia.com/v1` | `NVIDIA_API_KEY` |
| `ollama` | Ollama | `openai-chat` | `http://localhost:11434/v1` | `OLLAMA_API_KEY` (optional) |
| `ollama-cloud` | Ollama Cloud | `ollama-chat` | `https://ollama.com` | `OLLAMA_CLOUD_API_KEY`, `OLLAMA_API_KEY` |
| `openai` | OpenAI | `openai-responses` | `https://api.openai.com/v1` | `OPENAI_API_KEY` |
| `openai-codex` | ChatGPT (Codex) | `openai-responses` | `https://chatgpt.com/backend-api/codex` | none: sign-in only |
| `opencode-go` | OpenCode Go | `openai-chat` | `https://opencode.ai/inference/go/openai/v1` | `OPENCODE_API_KEY` |
| `opencode-zen` | OpenCode Zen | `openai-chat` | `https://opencode.ai/inference/openai/v1` | `OPENCODE_API_KEY` |
| `openrouter` | OpenRouter | `openrouter` | `https://openrouter.ai/api/v1` | `OPENROUTER_API_KEY`; sign-in |
| `otari` | Otari | `openai-chat` | `${OTARI_BASE_URL:-https://api.otari.ai/api/v1}` | `OTARI_API_KEY` |
| `qianfan` | Qianfan | `openai-chat` | `https://qianfan.baidubce.com/v2` | `QIANFAN_API_KEY` |
| `qwen-portal` | Qwen Portal | `openai-chat` | `https://portal.qwen.ai/v1` | `QWEN_OAUTH_TOKEN`, `QWEN_PORTAL_API_KEY` |
| `sakana` | Sakana AI | `openai-responses` | `https://api.sakana.ai/v1` | `SAKANA_API_KEY`, `FUGU_API_KEY` |
| `sambanova` | SambaNova | `openai-chat` | `https://api.sambanova.ai/v1` | `SAMBANOVA_API_KEY` |
| `scaleway` | Scaleway | `openai-chat` | `https://api.scaleway.ai/v1` | `SCW_SECRET_KEY`, `SCW_API_KEY`, `SCALEWAY_API_KEY` |
| `siliconflow` | SiliconFlow | `openai-chat` | `https://api.siliconflow.com/v1` | `SILICONFLOW_API_KEY` |
| `siliconflow-cn` | SiliconFlow CN | `openai-chat` | `https://api.siliconflow.cn/v1` | `SILICONFLOW_CN_API_KEY` |
| `singularityapi-dev` | SingularityAPI (pay as you go) | `openai-chat` | `https://api.singularityapi.dev/v1` | `SINGULARITYAPI_DEV_API_KEY` |
| `singularityapi-tech` | SingularityAPI (reserved lanes) | `openai-chat` | `https://api.singularityapi.tech/v1` | `SINGULARITYAPI_TECH_API_KEY` |
| `snowflake` | Snowflake Cortex | `openai-chat` | `https://${SNOWFLAKE_ACCOUNT}.snowflakecomputing.com/api/v2/cortex/v1` | `SNOWFLAKE_PAT`, `SNOWFLAKE_CORTEX_PAT` |
| `stepfun` | StepFun | `openai-chat` | `https://api.stepfun.ai/v1` | `STEPFUN_API_KEY` |
| `synthetic` | Synthetic | `openai-chat` | `https://api.synthetic.new/openai/v1` | `SYNTHETIC_API_KEY` |
| `together` | Together | `openai-chat` | `https://api.together.xyz/v1` | `TOGETHER_API_KEY` |
| `umans` | Umans AI Coding Plan | `openai-chat` | `https://api.code.umans.ai/v1` | `UMANS_AI_CODING_PLAN_API_KEY` |
| `venice` | Venice | `openai-chat` | `https://api.venice.ai/api/v1` | `VENICE_API_KEY` |
| `vercel-ai-gateway` | Vercel AI Gateway | `openai-chat` | `https://ai-gateway.vercel.sh/v1` | `AI_GATEWAY_API_KEY`, `VERCEL_AI_GATEWAY_API_KEY` |
| `vllm` | vLLM | `openai-chat` | `${VLLM_BASE_URL:-http://localhost:8000/v1}` | `VLLM_API_KEY` (optional) |
| `wafer-serverless` | Wafer Serverless | `openai-chat` | `https://pass.wafer.ai/v1` | `WAFER_SERVERLESS_API_KEY`, `WAFER_API_KEY` |
| `xai` | xAI | `openai-responses` | `https://api.x.ai/v1` | `XAI_API_KEY` |
| `xai-oauth` | xAI Grok (SuperGrok) | `openai-responses` | `https://api.x.ai/v1` | `XAI_OAUTH_TOKEN`; sign-in |
| `xiaomi` | Xiaomi MiMo | `openai-chat` | `https://api.xiaomimimo.com/v1` | `XIAOMI_API_KEY` |
| `xiaomi-token-plan-ams` | Xiaomi Token Plan (AMS) | `openai-chat` | `https://token-plan-ams.xiaomimimo.com/v1` | `XIAOMI_TOKEN_PLAN_AMS_API_KEY` |
| `xiaomi-token-plan-cn` | Xiaomi Token Plan (CN) | `openai-chat` | `https://token-plan-cn.xiaomimimo.com/v1` | `XIAOMI_TOKEN_PLAN_CN_API_KEY` |
| `xiaomi-token-plan-sgp` | Xiaomi Token Plan (SGP) | `openai-chat` | `https://token-plan-sgp.xiaomimimo.com/v1` | `XIAOMI_TOKEN_PLAN_SGP_API_KEY` |
| `yolo-auto` | Yolo-Auto | `openai-chat` | `https://yolo-auto.com/v1` | `YOLO_AUTO_API_KEY` |
| `zai` | Z.AI (GLM Coding Plan) | `openai-chat` | `https://api.z.ai/api/coding/paas/v4` | `ZAI_API_KEY` |
| `zenmux` | ZenMux | `openai-chat` | `https://zenmux.ai/api/v1` | `ZENMUX_API_KEY` |
| `zhipu-coding-plan` | Zhipu Coding Plan | `openai-chat` | `https://open.bigmodel.cn/api/coding/paas/v4` | `ZHIPU_API_KEY` |
| `bedrock` | Amazon Bedrock | `bedrock-converse` | AWS SDK | AWS credential chain |
| `vertex` | Google Vertex AI | per model | Google Cloud | Application default credentials |

Notes:

- Key variables are tried in the order shown. "(optional)" marks a server that is called without a key when none is set (the local ones); "sign-in" marks a provider `azazel login <id>` can sign in to.
- `${VAR}` in an address is filled from the environment, and `${VAR:-default}` falls back to the default. A provider whose address still needs a variable (`cloudflare-ai-gateway`, `cloudflare-workers-ai`, `snowflake`, `azure`) is listed, and answers a request with which variable to set.
- `ollama` chats through Ollama's OpenAI-compatible endpoint and reads the model list from its native one. Set `api = "ollama-chat"` on it to chat through the native API too.
- A provider with two surfaces has one preset here. To use the other, define a second provider, for example `api: anthropic-messages` with `base_url: https://api.z.ai/api/anthropic`, `https://zenmux.ai/api/anthropic`, `https://api.synthetic.new/anthropic` or `https://opencode.ai/inference/anthropic`.
- `kimi-code` may refuse clients it does not recognise; azazel sends its own identity.
- `github-copilot` also accepts a GitHub token with Copilot access in `COPILOT_GITHUB_TOKEN`. It asks each model on the API Copilot serves it on: Claude models (`claude-*`) on `anthropic-messages` (`/v1/messages`), GPT-5 and later (but `gpt-5-mini`), Grok and MAI Code models on `openai-responses` (`/responses`), the rest on `openai-chat`. Every route is told whether you or the agent started the turn, which decides how Copilot bills it.
- `cloudflare-ai-gateway` uses the gateway's `/compat` endpoint, which keeps the provider keys stored in the gateway and its dynamic routes. Cloudflare's newer REST endpoint, billed to the Cloudflare account, is the address of `cloudflare-workers-ai`, which serves third-party models too.

### Providers from models.dev

Beside the providers above, azazel knows every provider [models.dev](https://models.dev) lists that speaks `anthropic-messages`, `openai-responses`, `openai-chat` or `google-generative` and that no row above covers. Their rows are written by `cargo xtask catalog` into `crates/azazel-ai/src/providers/generated.rs`, with the models each serves in the catalogue; a row above with the same id, or taking its models from the same models.dev provider, replaces the generated one. Set the key variable and the provider's models appear in `/model` and `azazel models`.

models.dev names one route for each model: a wire API at an address. A provider whose models take several routes gets the one most of them take (on a tie: `anthropic-messages`, then `openai-responses`, then `openai-chat`), and the catalogue lists the models of that route; `/settings` and `azazel init` ask the provider itself for its whole list ([model discovery](#model-discovery)). To reach another route, define a second provider as above.

Each generated provider reads a key variable of its own: a variable models.dev gives to several providers, or that a provider above reads, is left out, so one key never makes several providers usable at once, and a provider left without one reads `<ID>_API_KEY` (`MINIMAX_CN_API_KEY` for `minimax-cn`). A provider on this machine (`localhost`) is called without a key when none is set.

<!-- generated by `cargo xtask catalog`: do not edit by hand -->

| Id | Name | Wire API | Base URL | Key variable |
|---|---|---|---|---|
| `302ai` | 302.AI | `openai-chat` | `https://api.302.ai/v1` | `302AI_API_KEY` |
| `abacus` | Abacus | `openai-chat` | `https://routellm.abacus.ai/v1` | `ABACUS_API_KEY` |
| `above` | above.dev | `openai-chat` | `https://api.above.dev/v1` | `ABOVE_API_KEY` |
| `agentrouter` | AgentRouter | `anthropic-messages` | `https://agentrouter.org` | `AGENTROUTER_API_KEY` |
| `agnes` | Agnes AI | `openai-chat` | `https://apihub.agnes-ai.com/v1` | `AGNES_API_KEY` |
| `ai-router` | AI-ROUTER | `openai-chat` | `https://api.ai-router.dev/v1` | `AI_ROUTER_API_KEY` |
| `ai21` | AI21 Labs | `openai-chat` | `https://api.ai21.com/studio/v1` | `AI21_API_KEY` |
| `ainetcafe` | ainetcafe | `openai-chat` | `https://microquickjs.com/v1` | `AINETCAFE_API_KEY` |
| `aixy` | Aixy | `openai-responses` | `https://api.aixy-gateway.com/v1` | `AIXY_API_KEY` |
| `aki-io` | AKI.IO | `openai-chat` | `https://aki.io/v1` | `AKI_IO_API_KEY` |
| `alibaba` | Alibaba | `openai-chat` | `https://dashscope-intl.aliyuncs.com/compatible-mode/v1` | `ALIBABA_API_KEY` |
| `alibaba-cn` | Alibaba (China) | `openai-chat` | `https://dashscope.aliyuncs.com/compatible-mode/v1` | `ALIBABA_CN_API_KEY` |
| `alibaba-coding-plan-cn` | Alibaba Coding Plan (China) | `openai-chat` | `https://coding.dashscope.aliyuncs.com/v1` | `ALIBABA_CODING_PLAN_CN_API_KEY` |
| `alibaba-token-plan-cn` | Alibaba Token Plan (China) | `openai-chat` | `https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1` | `ALIBABA_TOKEN_PLAN_CN_API_KEY` |
| `ambient` | Ambient | `openai-chat` | `https://api.ambient.xyz/v1` | `AMBIENT_API_KEY` |
| `amd` | AMD | `openai-chat` | `https://developer.amd.com.cn/radeon/api/v1` | `AMD_API_KEY` |
| `anyapi` | AnyAPI | `openai-chat` | `https://api.anyapi.ai/v1` | `ANYAPI_API_KEY` |
| `arcee` | Arcee | `openai-chat` | `https://api.arcee.ai/api/v1` | `ARCEE_API_KEY` |
| `atomic-chat` | Atomic Chat | `openai-chat` | `http://127.0.0.1:1337/v1` | `ATOMIC_CHAT_API_KEY` (optional) |
| `auriko` | Auriko | `openai-chat` | `https://api.auriko.ai/v1` | `AURIKO_API_KEY` |
| `azure-cognitive-services` | Azure Cognitive Services | `anthropic-messages` | `https://${AZURE_COGNITIVE_SERVICES_RESOURCE_NAME}.services.ai.azure.com/anthropic` | `AZURE_COGNITIVE_SERVICES_API_KEY` |
| `bailing` | Bailing | `openai-chat` | `https://api.tbox.cn/api/llm/v1` | `BAILING_API_TOKEN` |
| `bee` | Bee by HEOSSI | `openai-chat` | `https://api.bee.heossi.com/bee` | `BEE_API_KEY` |
| `berget` | Berget.AI | `openai-chat` | `https://api.berget.ai/v1` | `BERGET_API_KEY` |
| `blueclaw` | Blue Claw | `openai-chat` | `https://openai.blueclaw.network/v1` | `BLUECLAW_API_KEY` |
| `bothub` | Bothub | `openai-chat` | `https://openai.bothub.ru/v1` | `BOTHUB_API_KEY` |
| `boundless` | Boundless | `openai-chat` | `https://api.inference.boundless.network/v1` | `BOUNDLESS_API_KEY` |
| `byteplus` | BytePlus ModelArk | `openai-chat` | `https://ark.ap-southeast.bytepluses.com/api/v3` | `BYTEPLUS_API_KEY` |
| `chutes` | Chutes | `openai-chat` | `https://llm.chutes.ai/v1` | `CHUTES_API_KEY` |
| `clarifai` | Clarifai | `openai-chat` | `https://api.clarifai.com/v2/ext/openai/v1` | `CLARIFAI_PAT` |
| `claudinio` | Claudinio | `openai-chat` | `https://api.claudin.io/v1` | `CLAUDINIO_API_KEY` |
| `cloudferro-sherlock` | CloudFerro Sherlock | `openai-chat` | `https://api-sherlock.cloudferro.com/openai/v1` | `CLOUDFERRO_SHERLOCK_API_KEY` |
| `cohere` | Cohere | `openai-chat` | `https://api.cohere.ai/compatibility/v1` | `COHERE_API_KEY` |
| `cortecs` | Cortecs | `openai-chat` | `https://api.cortecs.ai/v1` | `CORTECS_API_KEY` |
| `crossmodel` | CrossModel | `openai-chat` | `https://api.crossmodel.ai/v1` | `CROSSMODEL_API_KEY` |
| `crusoe` | Crusoe | `openai-chat` | `https://api.inference.crusoecloud.com/v1` | `CRUSOE_API_KEY` |
| `daoxe` | DaoXE | `openai-chat` | `https://daoxe.com/v1` | `DAOXE_API_KEY` |
| `databricks` | Databricks | `openai-chat` | `https://${DATABRICKS_HOST}/ai-gateway/mlflow/v1` | `DATABRICKS_TOKEN` |
| `digitalocean` | DigitalOcean | `openai-chat` | `https://inference.do-ai.run/v1` | `DIGITALOCEAN_ACCESS_TOKEN` |
| `dinference` | DInference | `openai-chat` | `https://api.dinference.com/v1` | `DINFERENCE_API_KEY` |
| `drun` | D.Run (China) | `openai-chat` | `https://chat.d.run/v1` | `DRUN_API_KEY` |
| `ebcloud` | EBCloud | `openai-chat` | `https://maas-api.ebcloud.com/v1` | `EBCLOUD_API_KEY` |
| `echo` | Echo | `openai-chat` | `https://echo.tracerml.ai/v1` | `ECHO_API_KEY` |
| `edenai` | Eden AI | `openai-chat` | `https://api.edenai.run/v3` | `EDENAI_API_KEY` |
| `empiriolabs` | EmpirioLabs AI | `openai-chat` | `https://api.empiriolabs.ai/v1` | `EMPIRIOLABS_API_KEY` |
| `engy` | engy | `openai-chat` | `https://api.engy.ai/v1` | `ENGY_API_KEY` |
| `evroc` | evroc | `openai-chat` | `https://models.think.evroc.com/v1` | `EVROC_API_KEY` |
| `fastrouter` | FastRouter | `openai-chat` | `https://go.fastrouter.ai/api/v1` | `FASTROUTER_API_KEY` |
| `freemodel` | FreeModel | `anthropic-messages` | `https://cc.freemodel.dev` | `FREEMODEL_API_KEY` |
| `friendli` | Friendli | `openai-chat` | `https://api.friendli.ai/serverless/v1` | `FRIENDLI_TOKEN` |
| `frogbot` | FrogBot | `openai-chat` | `https://app.frogbot.ai/api/v1` | `FROGBOT_API_KEY` |
| `greenpt` | GreenPT | `openai-chat` | `https://api.greenpt.ai/v1` | `GREENPT_API_KEY` |
| `helicone` | Helicone | `openai-chat` | `https://ai-gateway.helicone.ai/v1` | `HELICONE_API_KEY` |
| `hetzner` | Hetzner | `openai-chat` | `https://inference.hetzner.com/api/v1` | `HETZNER_API_KEY` |
| `hopper` | Hopper | `openai-chat` | `https://api.withhopper.com/v1` | `HOPPER_API_KEY` |
| `hpc-ai` | HPC-AI | `openai-chat` | `https://api.hpc-ai.com/inference/v1` | `HPC_AI_API_KEY` |
| `impossibl` | Impossibl | `openai-chat` | `https://api.impossibl.com/v1` | `IMPOSSIBL_API_KEY` |
| `inceptron` | Inceptron | `openai-chat` | `https://api.inceptron.io/v1` | `INCEPTRON_API_KEY` |
| `inco` | Inco | `openai-chat` | `https://api.inco.ai/v1` | `INCO_API_KEY` |
| `infer` | Infer by Flow7 | `openai-responses` | `https://infer.flow7.org/v1` | `INFER_API_KEY` |
| `inference` | Inference | `openai-chat` | `https://inference.net/v1` | `INFERENCE_API_KEY` |
| `inferx` | InferX | `openai-chat` | `https://model.inferx.net/endpoints/v1` | `INFERX_API_KEY` |
| `infomaniak` | Infomaniak | `openai-chat` | `https://api.infomaniak.com/2/ai/${INFOMANIAK_PRODUCT_ID}/openai/v1` | `INFOMANIAK_API_KEY` |
| `io-net` | IO.NET | `openai-chat` | `https://api.intelligence.io.solutions/api/v1` | `IOINTELLIGENCE_API_KEY` |
| `iteracompute` | IteraCompute | `openai-chat` | `https://api.iteracompute.com/v1` | `ITERACOMPUTE_API_KEY` |
| `jalapeno` | Jalapeno Cloud | `openai-chat` | `https://api.jalapeno-cloud.ai/v1` | `JALAPENO_API_KEY` |
| `jiekou` | Jiekou.AI | `openai-chat` | `https://api.jiekou.ai/openai` | `JIEKOU_API_KEY` |
| `kenari` | Kenari | `openai-chat` | `https://kenari.id/v1` | `KENARI_API_KEY` |
| `kimi-code-plan-global` | Kimi For Coding (kimi.ai) | `openai-chat` | `https://api.kimi.ai/coding/v1` | `KIMI_CODE_PLAN_GLOBAL_API_KEY` |
| `klokintegration` | klokintegration.se | `openai-chat` | `https://api-gw.klok.ipaas.se/proxy/kloker-key/v1` | `KLOKINTEGRATION_API_KEY` |
| `kosmik` | Kosmik Compute | `openai-chat` | `https://api.koscompute.com/v1` | `KOSMIK_API_KEY` |
| `kuae-cloud-coding-plan` | KUAE Cloud Coding Plan | `openai-chat` | `https://coding-plan-endpoint.kuaecloud.net/v1` | `KUAE_API_KEY` |
| `lilac` | Lilac | `openai-chat` | `https://api.getlilac.com/v1` | `LILAC_API_KEY` |
| `liquid` | Liquid Inference | `openai-chat` | `https://router.inference.ai.exchange/v1` | `LIQUID_API_KEY` |
| `llmgateway` | DevPass (LLM Gateway) | `openai-chat` | `https://api.llmgateway.io/v1` | `LLMGATEWAY_API_KEY` |
| `llmgateway-providers` | LLM Gateway | `openai-chat` | `https://api.llmgateway.io/v1` | `LLMGATEWAY_PROVIDERS_API_KEY` |
| `llmtech` | LLM Tech | `openai-chat` | `https://api.llmtech.eu/v1` | `LLMTECH_API_KEY` |
| `llmtr` | LLMTR | `openai-chat` | `https://llmtr.com/v1` | `LLMTR_API_KEY` |
| `longcat` | LongCat | `openai-chat` | `https://api.longcat.chat/openai` | `LONGCAT_API_KEY` |
| `lowrouter` | LowRouter | `openai-chat` | `https://api.lowrouter.ai/v1` | `LOWROUTER_API_KEY` |
| `lucidquery` | LucidQuery | `openai-chat` | `https://api.lucidquery.com/v1` | `LUCIDQUERY_API_KEY` |
| `lynkr` | Lynkr | `openai-chat` | `http://127.0.0.1:8081/v1` | `LYNKR_API_KEY` (optional) |
| `meganova` | Meganova | `openai-chat` | `https://api.meganova.ai/v1` | `MEGANOVA_API_KEY` |
| `melious` | Melious | `openai-chat` | `https://api.melious.ai/v1` | `MELIOUS_API_KEY` |
| `minimax-cn` | MiniMax (minimax.cn) | `anthropic-messages` | `https://api.minimax.cn/anthropic` | `MINIMAX_CN_API_KEY` |
| `mixlayer` | Mixlayer | `openai-chat` | `https://models.mixlayer.ai/v1` | `MIXLAYER_API_KEY` |
| `moark` | Moark | `openai-chat` | `https://moark.com/v1` | `MOARK_API_KEY` |
| `modal` | Modal | `openai-chat` | `https://inference.us-west.modal.direct/v1` | `MODAL_PROXY_TOKEN` |
| `model-oracle-ai` | Model Oracle AI | `openai-chat` | `https://api.modeloracle.com/api/v1` | `MODEL_ORACLE_API_KEY` |
| `modelis` | Modelis | `openai-chat` | `https://modelishub.com/v1` | `MODELIS_API_KEY` |
| `modelscope` | ModelScope | `openai-chat` | `https://api-inference.modelscope.cn/v1` | `MODELSCOPE_API_KEY` |
| `moonshotai-cn` | Moonshot AI (China) | `openai-chat` | `https://api.moonshot.cn/v1` | `MOONSHOTAI_CN_API_KEY` |
| `morph` | Morph | `openai-chat` | `https://api.morphllm.com/v1` | `MORPH_API_KEY` |
| `munito` | Munito AI | `openai-chat` | `https://api.munito.ai/inference/v1` | `MUNITO_API_KEY` |
| `munito-eu` | Munito AI (EU) | `openai-chat` | `https://api.eu.munito.ai/inference/v1` | `MUNITO_EU_API_KEY` |
| `nan` | NaN | `openai-chat` | `https://api.nan.builders/v1` | `NAN_API_KEY` |
| `nearai` | NEAR AI Cloud | `openai-chat` | `https://cloud-api.near.ai/v1` | `NEARAI_API_KEY` |
| `nebius` | Nebius Token Factory | `openai-chat` | `https://api.tokenfactory.nebius.com/v1` | `NEBIUS_API_KEY` |
| `neon` | Neon | `openai-chat` | `${NEON_AI_GATEWAY_BASE_URL}/v1` | `NEON_AI_GATEWAY_TOKEN` |
| `neosmith` | NeoSmith | `openai-responses` | `https://router.neosmith.ai/v1` | `NEOSMITH_API_KEY` |
| `neuralwatt` | Neuralwatt | `openai-chat` | `https://api.neuralwatt.com/v1` | `NEURALWATT_API_KEY` |
| `nova` | Nova | `openai-chat` | `https://api.nova.amazon.com/v1` | `NOVA_API_KEY` |
| `oci` | OCI Generative AI | `openai-chat` | `https://inference.generativeai.us-chicago-1.oci.oraclecloud.com/openai/v1` | `OCI_GENAI_API_KEY` |
| `ofox` | Ofox | `openai-chat` | `https://api.ofox.ai/v1` | `OFOX_API_KEY` |
| `openreason` | OpenReason | `openai-chat` | `https://api.openreason.app/v1` | `OPENREASON_API_KEY` |
| `opper` | Opper | `openai-chat` | `https://api.opper.ai/v3/compat` | `OPPER_API_KEY` |
| `orcarouter` | OrcaRouter | `openai-chat` | `https://api.orcarouter.ai/v1` | `ORCAROUTER_API_KEY` |
| `ovhcloud` | OVHcloud AI Endpoints | `openai-chat` | `https://oai.endpoints.kepler.ai.cloud.ovh.net/v1` | `OVHCLOUD_API_KEY` |
| `pareto` | Pareto Inference | `openai-chat` | `https://api.paretoinference.com/v1` | `PARETO_API_KEY` |
| `pendra` | Pendra | `openai-chat` | `https://api.pendra.ai/api/v1` | `PENDRA_API_KEY` |
| `perplexity-agent` | Perplexity Agent | `openai-responses` | `https://api.perplexity.ai/v1` | `PERPLEXITY_API_KEY` |
| `pioneer` | Pioneer | `openai-chat` | `https://api.fastino.ai/v1` | `PIONEER_API_KEY` |
| `poe` | Poe | `openai-chat` | `https://api.poe.com/v1` | `POE_API_KEY` |
| `poolside` | Poolside | `openai-chat` | `https://inference.poolside.ai/v1` | `POOLSIDE_API_KEY` |
| `prime` | Prime Inference | `openai-chat` | `https://api.pinference.ai/api/v1` | `PRIME_API_KEY` |
| `privatemode-ai` | Privatemode AI | `openai-chat` | `http://localhost:8080/v1` | `PRIVATEMODE_API_KEY` (optional) |
| `qihang-ai` | QiHang | `openai-chat` | `https://api.qhaigc.net/v1` | `QIHANG_API_KEY` |
| `qiniu-ai` | Qiniu | `openai-chat` | `https://api.qnaigc.com/v1` | `QINIU_API_KEY` |
| `reflection` | Reflection | `openai-chat` | `https://api.reflection.ai/openai/v1` | `REFLECTION_API_KEY` |
| `regolo-ai` | Regolo AI | `openai-chat` | `https://api.regolo.ai/v1` | `REGOLO_API_KEY` |
| `requesty` | Requesty | `openai-chat` | `https://router.requesty.ai/v1` | `REQUESTY_API_KEY` |
| `routing-run` | routing.run | `openai-chat` | `https://api.routing.run/v1` | `ROUTING_RUN_API_KEY` |
| `runinfra` | RunInfra | `openai-chat` | `https://api.runinfra.ai/v1` | `RUNINFRA_GATEWAY_KEY` |
| `sarvam` | Sarvam AI | `openai-chat` | `https://api.sarvam.ai/v1` | `SARVAM_API_KEY` |
| `scnet-token-plan` | SCNet Token Plan | `openai-chat` | `https://api.scnet.cn/api/llm/v1` | `SCNET_API_KEY` |
| `scx-ai` | SCX.ai | `openai-chat` | `https://api.scx.ai/v1` | `SCX_API_KEY` |
| `sensenova` | SenseNova (China) | `openai-chat` | `https://token.sensenova.cn/v1` | `SENSENOVA_API_KEY` |
| `stackit` | STACKIT | `openai-chat` | `https://api.openai-compat.model-serving.eu01.onstackit.cloud/v1` | `STACKIT_API_KEY` |
| `stepfun-ai-step-plan` | StepFun Step Plan (Global) | `openai-chat` | `https://api.stepfun.ai/step_plan/v1` | `STEPFUN_AI_STEP_PLAN_API_KEY` |
| `stepfun-step-plan` | StepFun Step Plan (China) | `openai-chat` | `https://api.stepfun.com/step_plan/v1` | `STEPFUN_STEP_PLAN_API_KEY` |
| `subconscious` | Subconscious | `anthropic-messages` | `https://api.subconscious.dev` | `SUBCONSCIOUS_API_KEY` |
| `submodel` | submodel | `openai-chat` | `https://llm.submodel.ai/v1` | `SUBMODEL_INSTAGEN_ACCESS_KEY` |
| `tempr` | Tempr Gateway | `openai-chat` | `https://api.temprhq.io/v1` | `TEMPR_API_KEY` |
| `tencent-coding-plan` | Tencent Coding Plan (China) | `openai-chat` | `https://api.lkeap.cloud.tencent.com/coding/v3` | `TENCENT_CODING_PLAN_API_KEY` |
| `tencent-token-plan` | Tencent Token Plan | `openai-chat` | `https://api.lkeap.cloud.tencent.com/plan/v3` | `TENCENT_TOKEN_PLAN_API_KEY` |
| `tencent-tokenhub` | Tencent TokenHub | `openai-chat` | `https://tokenhub.tencentmaas.com/v1` | `TENCENT_TOKENHUB_API_KEY` |
| `tensorx` | TensorX | `openai-chat` | `https://api.tensorx.ai/v1` | `TENSORX_API_KEY` |
| `the-grid-ai` | The Grid AI | `openai-chat` | `https://api.thegrid.ai/v1` | `THEGRID_API_KEY` |
| `thinkingmachines` | Thinking Machines | `anthropic-messages` | `https://tinker.thinkingmachines.dev/services/tinker-prod/anthropic/api` | `TINKER_API_KEY` |
| `tinfoil` | Tinfoil | `openai-chat` | `https://inference.tinfoil.sh/v1` | `TINFOIL_API_KEY` |
| `tokengo` | TokenGo | `openai-chat` | `https://api.tokengo.com/v1` | `TOKENGO_API_KEY` |
| `tokenrouter` | TokenRouter | `openai-chat` | `https://api.tokenrouter.com/v1` | `TOKENROUTER_API_KEY` |
| `trustedrouter` | TrustedRouter | `openai-chat` | `https://api.trustedrouter.com/v1` | `TRUSTEDROUTER_API_KEY` |
| `umans-ai` | Umans AI | `openai-chat` | `https://api.code.umans.ai/v1` | `UMANS_AI_API_KEY` |
| `unorouter` | UnoRouter | `openai-chat` | `https://api.unorouter.com/v1` | `UNOROUTER_API_KEY` |
| `upstage` | Upstage | `openai-chat` | `https://api.upstage.ai/v1/solar` | `UPSTAGE_API_KEY` |
| `vancine` | Vancine | `openai-chat` | `https://vancine.com/v1` | `VANCINE_API_KEY` |
| `vispark` | Vispark | `openai-chat` | `https://api.lab.vispark.in/v1` | `VISPARK_LAB_API_KEY` |
| `vivgrid` | Vivgrid | `openai-chat` | `https://api.vivgrid.com/v1` | `VIVGRID_API_KEY` |
| `volcengine` | Volcengine Ark | `openai-chat` | `https://ark.cn-beijing.volces.com/api/v3` | `ARK_API_KEY` |
| `volcengine-coding-plan` | Volcengine Ark Coding Plan | `openai-chat` | `https://ark.cn-beijing.volces.com/api/coding/v3` | `ARK_CODING_PLAN_API_KEY` |
| `vultr` | Vultr | `openai-chat` | `https://api.vultrinference.com/v1` | `VULTR_API_KEY` |
| `wallaby` | Wallaby | `openai-chat` | `https://api.wallabytoken.com/v1` | `WALLABY_API_KEY` |
| `wally` | Wally | `openai-chat` | `https://inference.runanywhere.ai/v1` | `WALLY_API_KEY` |
| `xpersona` | Xpersona | `openai-chat` | `https://www.xpersona.co/v1` | `XPERSONA_API_KEY` |
| `zeldoc` | Zeldoc | `openai-chat` | `https://api.zeldoc.ai/v1` | `ZELDOC_API_KEY` |
| `zenifra` | Zenifra | `openai-chat` | `https://ai.zenifra.com/v1` | `ZENIFRA_AI_KEY` |
| `zhipuai` | Zhipu AI | `openai-chat` | `https://open.bigmodel.cn/api/paas/v4` | `ZHIPUAI_API_KEY` |

<!-- end of the generated table -->

## Custom providers

Providers are configured in two places with the same keys: `[providers.<id>]` tables in `config.toml`, and `providers:` in [`models.yml`](#modelsyml). An entry overrides a built-in provider key by key, or defines a new one when `api` and `base_url` are both set (in any layer).

```toml
[providers.ollama]
base_url = "http://192.168.1.20:11434/v1"

[providers.gateway]
api = "openai-chat"
base_url = "https://llm.example.com/v1"
api_key_env = "GATEWAY_API_KEY"
headers = { "X-Team" = "platform" }
```

| Key | Meaning |
|---|---|
| `name` | What the provider is called in menus; its id by default |
| `api` | A [wire API](#wire-apis) |
| `base_url` | Endpoint root; may contain `${VAR}` and `${VAR:-default}` |
| `api_key_env` | Environment variable holding the key |
| `api_key_command` | Shell command whose stdout is the key; run once and cached until the provider answers 401 |
| `api_key` | The key itself. Prefer one of the two above for anything you share |
| `headers` | Extra headers on every request; a value may contain `${VAR}`, and a header whose variable is unset is not sent. `{session_id}` in a value becomes the id of the conversation the request belongs to (the session and the agent), or an id of the provider's own for a request of none (the model list) |
| `auth` | How a key is presented: `auto` (default: what the wire API prescribes, a bearer token for the OpenAI formats and Ollama, `x-api-key` for Anthropic, `x-goog-api-key` for Google), `bearer` (`Authorization: Bearer`, whatever the API), `api-key` (an `api-key` header) or `azure-cli` (a Microsoft Entra token from the Azure CLI as a bearer token, instead of any key; see [Azure](#bedrock-vertex-and-azure)) |
| `discovery` | How models are listed: `auto` (default: the wire API's own list), `ollama` (`/api/tags` then `/api/show` per model), `lm-studio` (`/api/v0/models`, falling back to the OpenAI list), `azure` (the deployments of an Azure OpenAI resource) or `off`. `true` and `false` mean `auto` and `off` |
| `extra_body` | Members written into every request body, replacing what azazel wrote under the same name: routing (`provider = { order = ["anthropic"] }` on OpenRouter, `providerOptions = { gateway = { only = ["bedrock"] } }` on Vercel) and vendor switches (`chat_template_kwargs = { enable_thinking = true }`) |
| `tls` | `{ cert, key, ca }`: a client certificate for a gateway that asks for one, and the authority the server's certificate must come from; see [client certificates](#client-certificates) |
| `timeout_secs`, `first_event_secs`, `stall_secs` | This provider's own limits; see [stalls](#stalls) |
| `region`, `profile` | `bedrock` only |
| `project`, `location` | `vertex` only |
| `oauth`, `oauth_headers` | `[providers.<id>.oauth]`: an OAuth sign-in that `azazel login <id>` then uses, and headers sent only with that sign-in. `flow` is `pkce` (a browser and a code, with `authorize_url` and `token_url`), `device` (a code entered on another page, with `device_url`), `implicit` (the token comes back in the address the browser returns to; `authorize_url` only) or `device-auth-code` (OpenAI's device sign-in: `device_url`, `device_poll_url`, `verification_url`, then a code exchange at `token_url` with `redirect_uri`); then `client_id`, `scopes` and the rest. `inputs = [{ name, prompt, env, optional, suffix }]` are asked before the sign-in starts (from the environment variable `env` when it is set): `{name}` in the addresses, the scopes and `api_base_url` becomes the answer, which must be a host name or a name of letters, digits, `-`, `_` and `.`, and is kept with the login. `api_base_url` is kept with the login as the address its requests go to |
| `account` | The label of the stored account to use when a session chooses none (see [several accounts](#several-accounts)); the default account when unset |

A new provider needs a key only when it names a key source. Providers in a project's files are ignored until the project is trusted.

Any string value may hold `{env:VAR}` and `{file:path}`, filled once when the file is read; see [references to the environment and to files](configuration.md#references-to-the-environment-and-to-files):

```toml
[providers.gateway]
api_key = "{file:~/.secrets/gateway.key}"
headers = { "X-Team" = "{env:TEAM}" }
```

`opencode-go` and `opencode-zen` send `x-opencode-session: {session_id}` and `x-opencode-client: azazel`: OpenCode's gateways refuse a request that names no session (`400 MissingSessionID`).

### Client certificates

A company gateway that requires mutual TLS gets the certificate in the provider's `tls` table. Each value is the path of a PEM file (relative to the file that names it, or under `~/`) or the PEM itself, which is what a `{file:}` reference fills in:

```toml
[providers.gateway.tls]
cert = "~/.certs/gateway-client.pem"   # the client certificate (its chain); may hold the key too
key = "~/.certs/gateway-client.key"    # its private key, unless it is in `cert`
ca = "{file:~/.certs/company-ca.pem}"  # the server's certificate must come from this authority
```

- The client is that provider's only: other providers keep the shared one.
- `ca` replaces the system's certificate store for this provider; without it the system's is used.
- A file that cannot be read, or text that is no certificate or key, stops azazel at start with the provider and the key named.
- `bedrock` and `vertex` connect with their own clients and refuse `tls`.

## models.yml

`~/.azazel/models.yml` defines providers together with their models. A trusted project may have its own at `<project>/.azazel/models.yml`.

```yaml
providers:
  gateway:                        # a new provider: needs api and base_url
    name: Team gateway
    api: openai-chat
    base_url: https://llm.example.com/v1
    api_key_env: GATEWAY_API_KEY  # or api_key_command: "pass show gateway", or api_key: sk-…
    auth: bearer
    headers:
      X-Team: platform
    discovery: off                # list only the models below
    models:
      - id: big-model
        name: Big Model
        context_window: 200000
        max_output: 32000
        vision: true
        reasoning: true
        thinking_levels: [low, medium, high]
        cost: { input: 3.0, output: 15.0, cache_read: 0.3, cache_write: 3.75 }
      - id: small-model           # every key but id is optional

  ollama:                         # a built-in provider: only what is named changes
    base_url: http://192.168.1.20:11434/v1
    models:
      - id: <local-model>
        context_window: 32768

  openrouter:
    extra_body:
      provider: { order: [anthropic], allow_fallbacks: false }
```

| Level | Key | Meaning |
|---|---|---|
| root | `providers` | Provider id to its entry. The only key |
| provider | the keys of [custom providers](#custom-providers) | Same meaning as in `[providers.<id>]` |
| provider | `models` | The provider's models. They are added to what its own list reports; with `discovery: off` they are the whole list |
| model | `id` | Required. The model is `<provider>/<id>` |
| model | `name` | Display name |
| model | `context_window`, `max_output` | Token limits; `max_output` may not exceed `context_window` |
| model | `vision`, `reasoning` | Capabilities |
| model | `thinking_levels` | The [thinking levels](#thinking-levels) the model accepts. A request for another level is made at the nearest accepted one below it (else the lowest above). Naming any level marks the model as reasoning |
| model | `cost` | `{ input, output, cache_read, cache_write }` in USD per million tokens; the last two default to 0 |

Merge order, later wins, key by key:

1. The built-in providers and the catalogue.
2. `~/.azazel/models.yml`
3. `~/.azazel/config.toml` (`[providers.*]`, `[models.*]`)
4. `<project>/.azazel/models.yml` (trusted projects only)
5. `<project>/.azazel/config.toml`, then `config.local.toml`
6. `AZAZEL_*` environment variables, then command-line flags

So within one directory `config.toml` overrides the `models.yml` beside it, and the project overrides the home. `headers` and `extra_body` merge by key; every other value replaces the one below.

A mistake stops the load with the file and the key, for example `…/models.yml: providers.gateway.models[0].context_window: invalid type: string "many", expected u64`. `azazel doctor` lists each `models.yml` it read with its provider and model counts, and reports a project file skipped for lack of trust.

## Credentials

For each request the provider's credential is the first of:

1. `--api-key <KEY>`: applies to the provider of the first `--model`, for this run only.
2. The provider's environment variables, in order.
3. The same variables in `~/.azazel/.env`, then in the project's `.env` (at the project root).
4. `api_key_command`.
5. `api_key`, a key written in the configuration.
6. The entry in `~/.azazel/auth.json`: a stored API key or a sign-in.
7. Nothing: keyless providers are called unauthenticated; any other fails with ``no credentials for <id>: set <ENV> or run `azazel login <id>` ``.

An environment key therefore wins over a stored sign-in for the same provider. `azazel doctor` shows where each provider's credential comes from, never the secret.

### Several accounts

`auth.json` can hold more than one key or sign-in per provider. The entry under the provider's id is the default account; the others are kept as `<provider>#<label>`. A label is made of letters, digits, `-`, `_` and `.`.

```sh
azazel login anthropic --as work        # a second Claude subscription, kept beside the first
azazel login groq --as team             # a second key
azazel logout anthropic --account work  # forget that one only
```

A sign-in whose e-mail address and organisation match an account that is already stored renews that account, whatever label was asked for.

Which account a request uses, first match:

1. The account chosen for the session: `/account` → "Use for this session", or the one the router moved to (below). The choice is recorded in the session and made again when the session is resumed.
2. `[providers.<id>] account = "<label>"`, which `/account` → "Make default" writes to `~/.azazel/config.toml`.
3. The default account.

`/account [provider]` lists the provider's accounts: which is in use, which is the default, until when one is out of allowance, and what each has left of its usage windows (`5h 42% (resets 14:05) · 7d 7% · 7d Fable 90%`). Pick one to use it for this session, make it the default, or sign it out; "Add another account" signs in or stores a key under a new label. An account picked there is asked even when it was marked as out of allowance. E-mail addresses are shown as `j***@example.com` unless `[usage] redact_email = false`.

Usage windows come from response headers and, for subscriptions, from the provider's usage endpoint (see [usage](#usage)). Anthropic subscriptions report a 5-hour and a 7-day window, a weekly window per model family where the plan has one (`7d Fable`), which of them a refusal is about, and whether extra usage is on. A window that counts one model family says nothing about requests to other models: an account whose Fable allowance is used up still answers for Opus and Sonnet.

**When an account runs out.** A response that says the account's allowance is used up (Anthropic: a 429 whose unified rate-limit status is `rejected`, or the 400 about extra usage; ChatGPT: `usage_limit_reached`), or a rate limit that asks to wait more than a minute, marks that account as out of allowance until the reset time the provider gave (15 minutes when it gave none): for the models the window that ran out counts, or for all of them. If the provider has another stored account with room for the model, azazel selects it and asks the same model again. The line names the account and the window that ran out:

```text
anthropic: account work reached its 5-hour limit (resets 14:05); now using personal
anthropic: account default reached its 7-day Fable limit (resets Tue 09:00); now using work
```

Otherwise the chain falls back as for any other failure, and when no model of the chain can answer either, azazel asks what to do (see [when every account is out](#when-every-account-is-out)). An account is never switched for any other reason, and work stays on the account it moved to. Accounts apply to credentials in `auth.json` only: a key from the environment, a command or the configuration is used whatever is selected.

**Which account is next.** The first with room, in this order: the accounts `[accounts] order` lists for the provider, as listed; then the account a new session starts on (`[providers.<id>] account`, else the default account); then the rest by label.

```toml
[accounts]
order = { anthropic = ["work", "personal"] }
```

An account has room for a model when it is not marked as out of allowance for that model and the last usage report shows no window that the provider refuses for and that counts the model.

**Before a request.** The same check is made before a request is sent: when the account in use is known to be out of allowance for the model and another stored account has room, the request goes there without asking the first one, and the same line is shown. When no account has room the request is sent on the account in use, and what the provider answers counts, not what azazel remembered.

**Across runs.** The marks are kept in `~/.azazel/cache/limits.json` with their reset times, so a new run does not start on an account that ran out an hour ago. A mark ends at its reset time, when the account answers a request for a model it held back, when you pick the account in `/account`, or when a usage report fetched at least a minute after the refusal shows room in the window that ran out.

**In the background.** While a session is open, azazel asks the usage endpoint of every stored subscription login every `[usage] poll_secs` seconds (300; never less than 60; `0` turns it off), so that the windows of the accounts that are not in use are known too. A login is never renewed for this: an account whose token has run out is asked again after a request has renewed it. Accounts with an API key have no usage endpoint; for them azazel knows only what their responses said.

### When every account is out

When a request fails because a subscription is out of allowance, no other stored account of that provider has room and no other model of the chain answers, the run stops and asks:

```text
╭─ Usage limit reached ────────────────────────────────────────────────────────────────╮
│ anthropic: 5-hour limit reached until 14:05. You have 1 banked reset.                │
│                                                                                      │
│ Account default (y***@example.com).                                                  │
│ Session reset: Gives the 5-hour window back now. The weekly windows stay as they     │
│ are. There is one a week.                                                            │
│ Spending a reset cannot be undone.                                                   │
│                                                                                      │
│ ▌ Use the reset                                                                      │
│   Wait until reset                                                                   │
│   Pick another model                                                                 │
╰─ ↑↓ move · enter pick · esc cancel ──────────────────────────────────────────────────╯
```

| Answer | What happens |
|---|---|
| Use the reset | Offered when the account holds a [banked reset](#banked-resets) that gives back every window that is full. The reset is spent and the request that was refused is sent again |
| Wait until reset | The run waits until the reset time and then sends the request again. The line above the input counts down (`waiting for the 5-hour limit of anthropic · 2 h 10 min left`); `esc` stops the run. It goes on earlier when a usage report or another request shows the account has room again. Other agents that run into the same limit meanwhile wait with it, without asking again |
| Pick another model | A list of the roles and models that can be asked, without those the same limit holds back. The agent goes on with the one you pick, for this request and the ones after it; for the lead the choice is recorded in the session, as with `/model` |
| `esc` | The run fails with the provider's refusal, as it would without the question |

Before the question is put, azazel asks the provider for the account's usage and its resets, so the question says what holds now. When that report shows room (the window was reset elsewhere), the request is sent again without a question. The box names the account (its e-mail address shortened unless `[usage] redact_email = false`), the agent when it is not the lead, and what the reset gives back.

The question is asked through whatever is attached: the box in the UI, a `ui_request` of kind `select` (and a second one for the model) to an [RPC](rpc.md) client, a choice in an ACP client. It is asked for logins only: a provider used with an API key fails as before.

With nobody to ask (print mode, a background session, a schedule) the run fails as before and no reset is spent, unless `[usage] auto_reset = true`. Then a banked reset is spent without asking when all of this holds: the account holds one that gives back every window that is full; no earlier reset of the account is still open (see below); and no reset was tried for the account in the last five minutes.

### Banked resets

Some subscriptions hold resets: each gives a usage window back before its time, once.

| Provider | Reset | Gives back | Shown in `/usage` as |
|---|---|---|---|
| `openai-codex` (a ChatGPT login) | Reset credits of the plan | The windows of the plan | `2 banked resets · first lapses Tue 09:00` |
| `anthropic` (a Claude login) | The session reset: one a week, for accounts Anthropic offers it to | The 5-hour window; the weekly windows stay | `1 session reset`, `session reset used; the next one comes Tue 09:00`, `session resets: not offered to this account` |
| `anthropic` (a Claude login) | Weekly resets: grants an account earned | The windows the grant names, such as the weekly window of the Fable models; usable only while one of them is full | `2 weekly resets · first lapses Mon 08:59 · usable once the window it gives back is full` |

`/usage` and `azazel usage` ask the provider which resets an account holds when somebody looks and the last answer is older than five minutes; an answer older than an hour is no longer shown, so that nothing is offered from what was true yesterday, and what was known is dropped the moment a reset of the account is sent. Asking spends nothing.

`/reset-usage [provider]` spends one on purpose. It reads what the account holds, shows the reset (what it gives back, how many are left, when it lapses) and asks; the row a stray `enter` lands on is "Cancel". With several logins of the provider it asks which account; with several resets, which one.

A reset that is spent cannot be taken back, so every redemption goes the same way, whoever asked for it:

1. What the account holds is read fresh, and the account, the reset and a request id are fixed before the question is put. Switching accounts while the question is open cannot spend another account's reset.
2. The redemption is written to `~/.azazel/resets/<provider>.<account>.json` (the reset, the request id, the time; never a token) before it is sent. If that file cannot be written, nothing is sent.
3. It is sent once. azazel never sends it a second time by itself.
4. An answer that does not come (a timeout after 30 seconds, a lost connection, a server error, an answer that cannot be read) means the outcome is not known. azazel says so and does not count the reset as unspent. Only a request that never reached the provider, or one it turned away outright (HTTP 400, 401, 403, 404 or 422), is taken as not spent.
5. The usage report fetched afterwards settles it: when it shows room in the windows the reset was for, the redemption is closed. Until then it is open.

While a redemption of an account is open, no reset of that account is spent automatically. When its outcome is not known, the question and `/reset-usage` offer "Send the same reset request again" in place of a new reset: the same reset with the same request id, which a provider that already honoured it answers without spending another. An open redemption closes by itself when the reset it was about would have lapsed anyway, and after a week at the latest.

What the provider answers decides what is said: the window was reset; the reset was already spent; no window is full (nothing is spent); the account holds none or cannot use it (nothing is spent). A waiting request goes on after "reset" and after "no window is full"; after "already spent" and after an outcome that is not known, only when the usage report shows room. Otherwise the question is put again with what is known now.

Reading uses `GET /wham/rate-limit-reset-credits` on the ChatGPT backend, and Anthropic's usage endpoint with `?cedar_ember=1&skip_spend=1` (weekly resets) and `?at_wall=1&skip_spend=1` (the session reset). Spending uses `POST /wham/rate-limit-reset-credits/consume` and `POST /api/organizations/<organisation>/reset_rate_limits`. As with the usage endpoints, none of this is documented by the vendors; it is what their own clients do.

### Usage

`/usage` in the UI and `azazel usage [--json]` in the shell show what each signed-in subscription has left. In the UI it is a box in the middle of the window; in a terminal the command prints the same box, into a pipe (or with `NO_COLOR` set) the same rows as plain text:

```text
╭─ Usage ──────────────────────────────────────────────────────────────────────────────────────────╮
│ anthropic                                                                                        │
│  ● default  y***@example.com · max plan · in use                                                 │
│      5h        ━━━━━━━━────────────  42%  resets in 2 h 10 min (14:05)                           │
│      7d        ━━━━━━━━━━━━━━──────  71%  resets in 3 d 4 h (Tue 09:00)                          │
│      7d Fable  ━━━━━━━━━━━━━━━━━━━━ 100%  no requests for 3 d 4 h (Tue 09:00)  Fable only        │
│      extra usage off                                                                             │
│      Fable models are not asked on this account until Tue 09:00 (7-day Fable limit)              │
│    work  w***@example.com · pro plan                                                             │
│      5h        ━───────────────────   5%  resets in 4 h 31 min (16:26)                           │
│      7d        ━━━─────────────────  14%  resets in 6 d 2 h (Fri 07:00)                          │
│      7d Fable  ━━━━────────────────  20%  resets in 6 d 2 h (Fri 07:00)  Fable only              │
│      extra usage on · $12.50 of $50.00 used                                                      │
│      1 session reset                                                                             │
│                                                                                                  │
│ openai-codex                                                                                     │
│  ● default  y***@example.com · plus plan · in use                                                │
│      5h        ━───────────────────   5%  resets in 4 h 31 min (16:26)                           │
│      7d        ━━━─────────────────  14%  resets in 6 d 2 h (Fri 07:00)                          │
│      2 banked resets                                                                             │
╰─ r refresh · esc close ──────────────────────────────────────────────────────────────────────────╯
```

Per provider, one block per stored login, labelled accounts included:

- The account requests go to is marked (`●`, `in use`). The e-mail address is shown as `j***@example.com`; `[usage] redact_email = false` shows it whole.
- The plan, when the provider names it.
- Every window the provider reports, with a bar, the share used, the time to its reset and the time of day, and what it counts when that is not every request (`Fable only`). The bar turns to the warning colour from 80 % and to the error colour when the window is full; a window the provider refuses for says `no requests for 3 d 4 h` in place of `resets in`. A narrow box drops the note, then the time of day, then the bar; the share stays.
- Extra usage: on or off, and what it has cost when the provider says. While it is on and has money left, a full window does not stop requests and is not marked as refusing them.
- The [banked resets](#banked-resets) the account holds: how many, when the first lapses, and whether one can be spent now. `/reset-usage` spends one.
- Until when the account is left alone, and for which models (see [several accounts](#several-accounts)).

`r` asks the providers again; the arrows, `PgUp`/`PgDn`, `Home` and `End` scroll when the box is too short; `esc` or `enter` closes it. In a session that runs in another process (`azazel attach`, an RPC client) `/usage` answers with the same rows as text.

The numbers come from the provider: Anthropic is asked at `/api/oauth/usage` for a Claude login, the ChatGPT backend at `/wham/usage` for a ChatGPT login. Neither address is documented by its vendor; they are what the vendors' own clients ask. From Anthropic's answer azazel reads `five_hour`, `seven_day`, every row of `limits` (`weekly_scoped` rows are the weekly windows of single model families, named by `scope.model.display_name`), any other window member, and `spend` or `extra_usage`; from ChatGPT's, `plan_type`, `rate_limit`, `additional_rate_limits` (the windows of single models), `credits` and `rate_limit_reset_credits`.

An answer is kept in `~/.azazel/cache/limits.json`, per provider and account. `/usage` and `azazel usage` ask again when the answer is older than five minutes; `r` asks at once. When a provider cannot be asked (no network, an expired login, a changed endpoint), the block shows its last answer with its age and the reason, or, when there has never been one, what the headers of this session's responses said about the account. Providers used with an API key have no usage windows to show.

`azazel usage --json` prints one array with an object per account: `provider`, `account`, `email` (hidden the same way), `in_use`, `plan`, `windows` (`name`, `label`, `scope`, `used` from 0 to 1, `resets_at` in Unix seconds, `status`), `extra_usage`, `spend` (`used`, `limit`, `currency`), `resets` (`kind`: `credits`, `session` or `weekly`; `available`; `expires_at` and `next_available_at` in Unix seconds; `eligible`; `usable`), `blocks` (`until`, `since`, `window`, `scope`), `fetched_at` and `error`. The command exits with 1 when there are logins and nothing could be shown for any of them.

Between looks, the windows are kept up to date from response headers (Anthropic's unified rate-limit headers, its `7d_oi` headers for the weekly window of the Fable models included, and the `x-codex-primary-*` and `x-codex-secondary-*` headers of the ChatGPT backend) and by the background asking described above. While the lead answers on a subscription, the status line shows the fullest window that counts the lead's model in place of a cost (`◔ 61% 5h`), in the warning colour once it is at least 80 % full; see [the status line](tui.md#status-line).

A subscription whose window is full is refused by Anthropic with "You're out of extra usage", which names neither the window nor the wait. azazel adds what it knows as a second line:

```text
usage limit reached: You're out of extra usage. Add more at claude.ai/settings/usage and keep going.
the 5-hour limit of this account is full; it resets in 2 h 10 min
```

### Keys in `.env` files

A `.env` file holds `NAME=value` lines; `export NAME=value`, quoted values and `#` comments are understood.

```sh
# ~/.azazel/.env
ANTHROPIC_API_KEY=sk-ant-...
export OPENAI_API_KEY="sk-..."
```

- Only the variables a provider names as its key are taken from the file. Everything else in it is ignored.
- The values are used for requests to that provider and nothing else: they are not put into azazel's environment, so the commands agents run, hooks and MCP servers do not see them.
- The project's `.env` is read only when the project is trusted (or with `--trust`): a file in a repository you just cloned must not decide which account your requests go to.
- A variable set in the real environment wins over both files.

`azazel doctor` and `azazel auth status` show the file as the source: `OPENAI_API_KEY in /work/app/.env`.

`auth.json` is written with mode `0600` on Unix, by temp file and rename under a lock file (`auth.json.lock`), so several azazel processes can share it. Sign-in tokens are refreshed automatically when they have less than five minutes left.

`auth.json` holds the providers' keys and sign-ins and nothing else. The credentials `azazel secrets` stores for agents (the API keys the `http` tool adds to requests by host, and the site logins the `browser` tool types) live in a file of their own beside it, `~/.azazel/secrets.json`, written the same way: mode `0600` on Unix, temp file and rename under `secrets.json.lock`. See [credentials for services](web-and-browser.md#credentials-for-services) and [signing in](web-and-browser.md#signing-in).

`azazel auth status [--json]` lists each provider that has a credential and where it comes from (`--api-key`, an
environment variable, a `.env` file, `api_key_command`, the configuration, or `auth.json`), and for logins the
account and when the token expires. A provider with several stored accounts lists their labels, and its source names the one in
use (`auth.json (login, account work)`). Keys and tokens are never printed.

## Logging in and out

```sh
azazel login                  # lists providers and asks
azazel login groq             # asks for the key (not echoed) and stores it
azazel login anthropic        # browser sign-in with a Claude Pro or Max subscription
azazel login anthropic --key  # store an API key instead of signing in
azazel login openai-codex     # browser sign-in with a ChatGPT subscription
azazel login github-copilot   # enter a code on github.com
azazel login github-copilot --enterprise company.ghe.com   # through a GitHub Enterprise
azazel login openai-codex --device   # ChatGPT on a machine without a browser: enter a code on any device
azazel login digitalocean     # browser sign-in to DigitalOcean's inference API
azazel login poe              # creates a Poe API key in the browser (--key to paste one)
azazel login snowflake        # Snowflake's browser sign-in, beside SNOWFLAKE_PAT
azazel login openrouter       # creates an OpenRouter key in the browser (--key to paste one)
azazel logout anthropic       # forget the stored key or sign-in
azazel login anthropic --as work         # keep it as a second account (see "Several accounts")
azazel logout anthropic --account work   # forget that account only
```

| Provider | Sign-in | Notes |
|---|---|---|
| `anthropic` | Claude Pro or Max | Browser redirects to `localhost:54545`; if it cannot reach this machine, paste the address it ends up at |
| `openai-codex` | ChatGPT Plus or Pro | The redirect address is fixed: port 1455 must be free while the login runs |
| `openai-codex --device` | ChatGPT Plus or Pro, without a browser here | Open `auth.openai.com/codex/device` on any device, sign in and enter the code shown. The login is the same as the browser one's and renews the same way |
| `github-copilot` | GitHub Copilot | Device flow: enter the code shown on `github.com/login/device`. The token does not expire; revoke the app on GitHub to end it |
| `github-copilot --enterprise <domain>` | GitHub Copilot through a GitHub Enterprise | The same device flow at `https://<domain>/login/device`; requests go to `https://copilot-api.<domain>`, which the login keeps |
| `digitalocean` | A DigitalOcean account | The browser returns to `localhost:1456`, which must be free, with a token for the inference API. It cannot be renewed: sign in again when it runs out |
| `poe` | Poe's key flow for applications | Any free local port. What is stored is a Poe API key, valid as long as Poe says (or until revoked) |
| `snowflake` | A Snowflake user, through the browser | Asks for the account (`SNOWFLAKE_ACCOUNT` when set; `myorg-myaccount`) and a role (`SNOWFLAKE_ROLE`, or empty for the default role). The account is kept with the login, so `SNOWFLAKE_ACCOUNT` is not needed afterwards. `SNOWFLAKE_PAT` still wins when it is set |
| `google-gemini-cli` | A Google account, as the Gemini CLI signs in | Port 8085 must be free. The Cloud project requests are made in is looked up on first use and kept with the login; accounts on a paid tier set `GOOGLE_CLOUD_PROJECT` |
| `openrouter` | OpenRouter's key flow for applications | Port 54549. What is stored is a lasting API key |
| `xai-oauth` | SuperGrok or X Premium+ | Device flow at `auth.x.ai` |

A sign-in must be finished within ten minutes; `Ctrl+C` cancels it. Providers may restrict the use of subscription accounts from third-party clients; check their terms before relying on one.

`azazel login` reads providers from the built-in list and the files in `~/.azazel` only, so a project file cannot redirect where a key is sent.

In the TUI:

- `/login [provider] [account]` signs in, or asks for the key of a provider without a sign-in; with an account label it is kept as another account. `--device` and `--enterprise <domain>` work as on the command line. `/logout <provider> [account]` forgets it.
- `/account [provider]` lists a provider's accounts with their usage and switches between them.
- `/settings` has an Accounts page (sign in with a subscription, add an API key, sign out or forget a key, the credentials in use) and a Models page (the default model and the model of every role). After the first credential it goes straight on to choosing a default model. See [settings](tui.md#settings).

## Bedrock, Vertex and Azure

```toml
[providers.bedrock]          # both keys optional
region = "us-east-1"
profile = "work"

[providers.vertex]           # both keys optional
project = "my-gcp-project"
location = "us-east5"

[providers.azure]            # optional: the address can also come from the environment
base_url = "https://<resource>.openai.azure.com"
api = "azure-openai-chat"    # default: azure-openai-responses
auth = "azure-cli"           # optional: a Microsoft Entra token from `az` instead of a key
```

- **Bedrock** and **Vertex** are listed only when they are set up, which is judged without asking the network: Bedrock when `region` or `profile` is set, an AWS credential variable is (`AWS_ACCESS_KEY_ID`, `AWS_PROFILE`, `AWS_WEB_IDENTITY_TOKEN_FILE`, `AWS_CONTAINER_CREDENTIALS_RELATIVE_URI` or `_FULL_URI`), or `~/.aws/credentials` or `~/.aws/config` exists (or the files `AWS_SHARED_CREDENTIALS_FILE` and `AWS_CONFIG_FILE` name); Vertex when `project` is set, `GOOGLE_APPLICATION_CREDENTIALS`, `GOOGLE_CLOUD_PROJECT` or `GCLOUD_PROJECT` is, or the file `gcloud auth application-default login` writes exists. A model named in full works either way.
- **Bedrock** uses the AWS default credential chain and region unless `region` or `profile` is set. Models are `bedrock/<model id>`, for example `bedrock/anthropic.claude-haiku-4-5-20251001-v1:0`. Thinking is requested for Claude models only.
- **Vertex** uses Google application default credentials (`gcloud auth application-default login` or `GOOGLE_APPLICATION_CREDENTIALS`). `project` falls back to `GOOGLE_CLOUD_PROJECT`, `GCLOUD_PROJECT`, then the project of the credentials; `location` defaults to `global`. Model ids containing `claude` are sent to the Anthropic publisher (`vertex/claude-sonnet-4-5@20250929`), all others to the Google publisher.
- **Azure OpenAI** is the provider id `azure`. Its address is `AZURE_OPENAI_BASE_URL`, else `https://$AZURE_OPENAI_RESOURCE_NAME.openai.azure.com`, else `base_url`; a trailing `/openai` or `/openai/v1` is accepted. Its key comes from `AZURE_OPENAI_API_KEY` and is sent as an `api-key` header; for Microsoft Entra set `auth = "azure-cli"`: every request then carries a token from `az account get-access-token --resource https://cognitiveservices.azure.com` (run `az login` first), asked for again five minutes before it runs out and after the resource refuses one, and keys are not used. A model is a deployment: `azure/<deployment>`. When deployments are not named after their models, `AZURE_OPENAI_DEPLOYMENT_NAME_MAP="gpt-x=prod-x,gpt-y=prod-y"` maps them. `AZURE_OPENAI_API_VERSION` sets `api-version` (the chat API needs one and defaults to `2024-10-21`; the Responses API sends none unless it is set). The model list is the resource's deployments: with `azure-cli` and an address at `*.openai.azure.com`, `*.cognitiveservices.azure.com` or `*.services.ai.azure.com`, Azure Resource Manager (the resource found by name through Resource Graph in every subscription your account can read, then its deployments; a token for `https://management.azure.com` comes from `az` too); otherwise, or when that fails, the resource's own `GET /openai/deployments?api-version=2022-12-01` with the key or the token. A deployment is listed under its own name, with the model it serves as its display name when that differs. Deployments that are not finished are left out. The older form `api = "openai-responses"` with `base_url = "https://<resource>.openai.azure.com/openai/v1"` keeps working.
- **Amazon Bedrock through its OpenAI-compatible endpoint** is the provider `bedrock-mantle`: a Bedrock API key in `AWS_BEARER_TOKEN_BEDROCK`, region from `AWS_REGION` (default `us-east-1`). It needs no Cargo feature.

Bedrock and Vertex are Cargo features (`bedrock`, `vertex`), on by default.

## Thinking levels

Levels: `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`. The default is `off`.

| Set with | Scope |
|---|---|
| `--thinking <level>` | The lead, this run |
| `/thinking` (picker), `/thinking <level>`, `Alt+T`, RPC `set_thinking` | The lead, this session |
| `thinking:` in an agent definition, `[agents.<name>] thinking` | That agent (a primary agent: the lead while it is that agent) |
| `:level` suffix on a model spec (`[roles] smol = ["openai/gpt-5-mini:low"]`) | That model, wherever the spec is used, when none of the above chose a level |

A level chosen in one of the first three ways wins over a model's `:level`, so `/thinking low` lowers the
thinking of a lead whose role is written `:high`, and `/thinking off` turns it off. Until a level is chosen, each
request asks for the `:level` of the model it goes to, else `off`. The status line and the RPC `get_state` command
show the level the lead's requests go out at.

`/thinking` offers `off` and the levels the lead's model takes, as `Alt+T` steps through them (every level for a
model the catalogue names none for); `/thinking <level>` with a level the model does not take sets the nearest one
it does, and says so.

| Level | Anthropic, Google, Bedrock (token budget) | OpenAI formats, Azure (effort) | `openrouter` (effort) | `ollama-chat` (`think`) |
|---|---|---|---|---|
| `off` | none sent | none sent | none sent | `false` |
| `minimal` | 1024 | `minimal` | `minimal` | `low` |
| `low` | 4096 | `low` | `low` | `low` |
| `medium` | 10000 | `medium` | `medium` | `medium` |
| `high` | 24000 | `high` | `high` | `high` |
| `xhigh` | 48000 | `high` | `xhigh` | `high` |
| `max` | the model's `max_output` − 1024 | `high` | `xhigh` | `max` |

Budgets are capped at `max_output` − 1024. No thinking parameter is sent when the catalogue says the model does not reason. A model that lists `thinking_levels` is asked at the nearest level it accepts. `Ctrl+T` shows or hides thinking text in the TUI.

## Model catalogue

The catalogue supplies each model's name, context window, output limit, capabilities and prices. It is merged from, later wins:

1. The snapshot built into the binary (from models.dev; `cargo xtask catalog` regenerates it).
2. `~/.azazel/cache/models.json`, where the model list of a signed-in subscription provider is recorded.
3. The models of [`models.yml`](#modelsyml) and the `[models."<provider>/<id>"]` tables of `config.toml`, in the merge order given there.

```toml
[models."gateway/my-model"]
name = "My Model"
context_window = 200000
max_output = 32000
vision = true
reasoning = true
thinking_levels = ["low", "medium", "high"]
cost = { input = 3.0, output = 15.0, cache_read = 0.3, cache_write = 3.75 }  # USD per million tokens
```

Every key is optional. A model the catalogue does not know is looked up once in its provider's own model list; failing that it is assumed to have a 128000-token context window, 8192 output tokens, no vision, no reasoning and no price (so no cost is shown). Set `reasoning = true` for such a model if you want thinking levels to apply, and `vision = true` if it accepts images. When a provider reports what a response cost (OpenRouter and gateways like it), that amount is shown instead of one computed from the catalogue's prices.

### Model discovery

`/settings` and `azazel init` ask the chosen provider for its models; `/settings` waits up to 20 seconds. If the provider does not answer, or lists nothing, the catalogue's models for it are offered instead.

| `discovery` | Request | What is read |
|---|---|---|
| `auto`, OpenAI formats | `GET {base_url}/models` | `id`; context from `context_length`, `context_window`, `max_context_length`, `max_model_len`, `top_provider.context_length`, `capabilities.limits`, `meta.n_ctx`; output limit, vision, reasoning and `pricing` where the server reports them |
| `auto`, `anthropic-messages` | `GET {base_url}/v1/models` | `id`, limits and capabilities |
| `auto`, `google-generative` | `GET {base_url}/models`, every page | Models that generate content; `inputTokenLimit`, `outputTokenLimit`, `displayName`, `thinking` |
| `auto`, `google-gemini-cli` | `POST {base_url}/v1internal:retrieveUserQuota` | The models the account has quota for |
| `auto`, `ollama-chat` | `GET {root}/api/tags` | Names only |
| `ollama` | `GET {root}/api/tags`, then `POST {root}/api/show` per model | Context from the model's `num_ctx` parameter, else `OLLAMA_CONTEXT_LENGTH`, else its `*.context_length`; vision and reasoning from `capabilities`; embedding models are left out |
| `lm-studio` | `GET {root}/api/v0/models`, else the OpenAI list | The context a loaded model runs with, else its maximum; vision; embedding models are left out |
| `azure` (the `azure` provider) | With `auth = "azure-cli"`, Azure Resource Graph and `GET https://management.azure.com/<resource>/deployments`, every page; else `GET {resource}/openai/deployments?api-version=2022-12-01` | Deployments whose provisioning succeeded: the deployment name, and the model it serves |
| `off` | none | Only the models named in the configuration, else the catalogue's |

Models named for a provider in `models.yml` or `[models]` are always listed, whatever its endpoint reports.

See also [configuration](configuration.md) and [getting started](getting-started.md).
