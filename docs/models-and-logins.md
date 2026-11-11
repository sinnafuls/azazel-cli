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
| `default` | none: you must set it | The lead agent; `coder`, `tester`, `researcher`, `designer`, `artist` |
| `smol` | `@default` | `scout`, compaction summaries, `prompt` hooks, MCP sampling |
| `slow` | `@default` | `reviewer` |
| `plan` | `@slow` | `planner` |
| `verify` | `@default` | `verifier` (see [goals](goals.md)) |
| `memory` | `@smol` | Memory extraction (see [memory](memory.md)) |
| `vision` | `@default` | `playtester` |

You may define further roles and use them anywhere a spec is accepted. When a chain is resolved, roles are expanded in order, a model that already appeared is dropped, and an unknown role or a role that refers back to itself is skipped.

## Choosing the model

| Where | Effect |
|---|---|
| `azazel init` | Asks for a provider and model, writes `roles.default` to `~/.azazel/config.toml` |
| `/settings` → "Choose the default model" | Same, from inside the TUI; takes effect at once |
| `--model <spec>` | The lead's chain for this run; repeat the flag to add fallbacks |
| `/model` or `Ctrl+L` | Picker over roles and the models of providers that have a credential |
| `/model <spec> [fallback …]` | Sets the lead's chain for this session |
| `model:` in an agent definition, `[agents.<name>] model` | Per-agent chain (see [team](team.md)) |

When every `--model` is a concrete model, that chain also becomes this run's `default` role, so the roles and agents built on `@default` follow it.

```sh
azazel --model anthropic/claude-sonnet-4-5 --model ollama/<local-model> -p "summarise src/"
```

Without a resolvable model, print mode and the subcommands stop with `no model configured: run "azazel init" or pass --model provider/id`. The interactive UI starts anyway so you can use `/settings`.

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

## Built-in providers

| Id | Wire API | Base URL | Key variable |
|---|---|---|---|
| `anthropic` | `anthropic-messages` | `https://api.anthropic.com` | `ANTHROPIC_API_KEY` |
| `openai` | `openai-responses` | `https://api.openai.com/v1` | `OPENAI_API_KEY` |
| `google` | `google-generative` | `https://generativelanguage.googleapis.com/v1beta` | `GEMINI_API_KEY`, then `GOOGLE_API_KEY` |
| `openrouter` | `openai-chat` | `https://openrouter.ai/api/v1` | `OPENROUTER_API_KEY` |
| `groq` | `openai-chat` | `https://api.groq.com/openai/v1` | `GROQ_API_KEY` |
| `deepseek` | `openai-chat` | `https://api.deepseek.com` | `DEEPSEEK_API_KEY` |
| `xai` | `openai-chat` | `https://api.x.ai/v1` | `XAI_API_KEY` |
| `mistral` | `openai-chat` | `https://api.mistral.ai/v1` | `MISTRAL_API_KEY` |
| `ollama` | `openai-chat` | `http://localhost:11434/v1` | `OLLAMA_API_KEY` (optional) |
| `lmstudio` | `openai-chat` | `http://localhost:1234/v1` | `LMSTUDIO_API_KEY` (optional) |
| `llamacpp` | `openai-chat` | `http://localhost:8080/v1` | `LLAMACPP_API_KEY` (optional) |
| `openai-codex` | `openai-responses` | `https://chatgpt.com/backend-api/codex` | none: sign-in only |
| `bedrock` | `bedrock-converse` | AWS SDK | AWS credential chain |
| `vertex` | per model | Google Cloud | Application default credentials |

The three local presets send requests without a key when none is set.

## Custom providers

A `[providers.<id>]` table overrides a built-in provider field by field, or defines a new one when it has both `api` and `base_url`.

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
| `api` | `anthropic-messages`, `openai-responses`, `openai-chat`, `google-generative` |
| `base_url` | Endpoint root. Requests go to `{base_url}/v1/messages` (Anthropic), `{base_url}/responses`, `{base_url}/chat/completions`, or `{base_url}/models/{id}:streamGenerateContent` (Google) |
| `api_key_env` | Environment variable holding the key |
| `api_key_command` | Shell command whose stdout is the key; run once and cached until the provider answers 401 |
| `headers` | Extra headers on every request |
| `auth` | `bearer` (default) or `api-key` (an `api-key: <key>` header) for the OpenAI-format APIs |
| `region`, `profile` | `bedrock` only |
| `project`, `location` | `vertex` only |
| `oauth` | `[providers.<id>.oauth]`: an OAuth sign-in (`flow = "pkce"` or `"device"`, `client_id`, `token_url`, `authorize_url` or `device_url`, `scopes`, …) that `azazel login <id>` then uses |

A new provider needs a key only when it names a key source. A `[providers]` table in a project's config is ignored until the project is trusted.

## Credentials

For each request the provider's credential is the first of:

1. `--api-key <KEY>`: applies to the provider of the first `--model`, for this run only.
2. The provider's environment variable.
3. `api_key_command`.
4. The entry in `~/.azazel/auth.json`: a stored API key or a sign-in.
5. Nothing: keyless providers are called unauthenticated; any other fails with ``no credentials for <id>: set <ENV> or run `azazel login <id>` ``.

An environment key therefore wins over a stored sign-in for the same provider. `azazel doctor` shows where each provider's credential comes from, never the secret.

`auth.json` is written with mode `0600` on Unix, by temp file and rename under a lock file (`auth.json.lock`), so several azazel processes can share it. Sign-in tokens are refreshed automatically when they have less than five minutes left.

## Logging in and out

```sh
azazel login                 # lists providers and asks
azazel login openrouter      # asks for the key (not echoed) and stores it
azazel login anthropic       # browser sign-in with a Claude Pro or Max subscription
azazel login anthropic --key # store an API key instead of signing in
azazel login openai-codex    # browser sign-in with a ChatGPT subscription
azazel logout anthropic      # forget the stored key or sign-in
```

| Provider | Sign-in | Notes |
|---|---|---|
| `anthropic` | Claude Pro or Max | Browser redirects to `localhost:54545`; if it cannot reach this machine, paste the address it ends up at |
| `openai-codex` | ChatGPT Plus or Pro | The redirect address is fixed: port 1455 must be free while the login runs |

A sign-in must be finished within ten minutes; `Ctrl+C` cancels it. Providers may restrict the use of subscription accounts from third-party clients; check their terms before relying on one.

`azazel login` reads providers from the built-in list and `~/.azazel/config.toml` only, so a project file cannot redirect where a key is sent.

In the TUI:

- `/login [provider]` signs in, or asks for the key of a provider without a sign-in. `/logout <provider>` forgets it.
- `/settings` opens a menu: sign in with a subscription, add an API key, choose the default model, sign out or forget a key. After the first credential it goes straight on to choosing a default model.

## Bedrock, Vertex and Azure

```toml
[providers.bedrock]          # both keys optional
region = "us-east-1"
profile = "work"

[providers.vertex]           # both keys optional
project = "my-gcp-project"
location = "us-east5"

[providers.azure]
api = "openai-responses"     # or "openai-chat"
base_url = "https://<resource>.openai.azure.com/openai/v1"
```

- **Bedrock** uses the AWS default credential chain and region unless `region` or `profile` is set. Models are `bedrock/<model id>`, for example `bedrock/anthropic.claude-haiku-4-5-20251001-v1:0`. Thinking is requested for Claude models only.
- **Vertex** uses Google application default credentials (`gcloud auth application-default login` or `GOOGLE_APPLICATION_CREDENTIALS`). `project` falls back to `GOOGLE_CLOUD_PROJECT`, `GCLOUD_PROJECT`, then the project of the credentials; `location` defaults to `global`. Model ids containing `claude` are sent to the Anthropic publisher (`vertex/claude-sonnet-4-5@20250929`), all others to the Google publisher.
- **Azure OpenAI** is the provider id `azure`. Its key comes from `AZURE_OPENAI_API_KEY` and is sent as an `api-key` header. For Entra tokens set `auth = "bearer"` and an `api_key_command` that prints the token.

Bedrock and Vertex are Cargo features (`bedrock`, `vertex`), on by default.

## Thinking levels

Levels: `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`. The default is `off`.

| Set with | Scope |
|---|---|
| `--thinking <level>` | The lead, this run |
| `/thinking` (picker) or `/thinking <level>` | The lead, this session |
| `:level` suffix on a model spec | That model, wherever the spec is used; wins over the above |
| `thinking:` in an agent definition, `[agents.<name>] thinking` | One sub-agent |

| Level | Anthropic, Google, Bedrock (token budget) | OpenAI-format APIs (effort) |
|---|---|---|
| `minimal` | 1024 | `minimal` |
| `low` | 4096 | `low` |
| `medium` | 10000 | `medium` |
| `high` | 24000 | `high` |
| `xhigh` | 48000 | `high` |
| `max` | the model's `max_output` − 1024 | `high` |

Budgets are capped at `max_output` − 1024. No thinking parameter is sent when the level is `off` or the catalogue says the model does not reason. `Ctrl+T` shows or hides thinking text in the TUI.

## Model catalogue

The catalogue supplies each model's context window, output limit, capabilities and prices. It is merged from, later wins:

1. The snapshot built into the binary.
2. `~/.azazel/cache/models.json`, where the model list of a signed-in subscription provider is recorded.
3. `[models."<provider>/<id>"]` tables in config.

```toml
[models."gateway/my-model"]
context_window = 200000
max_output = 32000
vision = true
reasoning = true
cost = { input = 3.0, output = 15.0, cache_read = 0.3, cache_write = 3.75 }  # USD per million tokens
```

Every key is optional. A model the catalogue does not know is looked up once in its provider's own model list; failing that it is assumed to have a 128000-token context window, 8192 output tokens, no vision, no reasoning and no price (so no cost is shown). Set `reasoning = true` for such a model if you want thinking levels to apply, and `vision = true` if it accepts images.

See also [configuration](configuration.md) and [getting started](getting-started.md).
