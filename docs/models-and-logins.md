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
| `lead` | `@default` | The lead agent |
| `scout` | `@smol` | The `scout` agent |
| `planner` | `@plan` | The `planner` agent |
| `coder`, `tester`, `researcher`, `designer`, `artist` | `@default` | The agent of the same name |
| `reviewer` | `@slow` | The `reviewer` agent |
| `verifier` | `@verify` | The `verifier` agent |
| `playtester` | `@vision` | The `playtester` agent |

The lead starts on `@lead`, and each built-in agent on the role named after it, so any one of them can be given its own model without touching the others: `/settings` → "Models for the lead and each agent" writes `[roles] <name> = ["provider/model"]`, and you can write the same by hand.

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
| `google` | Google Gemini API | `google-generative` | `https://generativelanguage.googleapis.com/v1beta` | `GEMINI_API_KEY`, `GOOGLE_API_KEY` |
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
| `minimax-code-cn` | MiniMax Code CN | `anthropic-messages` | `https://api.minimaxi.com/anthropic` | `MINIMAX_CODE_CN_API_KEY` |
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
| `opencode-go` | OpenCode Go | `openai-chat` | `https://opencode.ai/zen/go/v1` | `OPENCODE_API_KEY` |
| `opencode-zen` | OpenCode Zen | `openai-chat` | `https://opencode.ai/zen/v1` | `OPENCODE_API_KEY` |
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
| `xai` | xAI | `openai-chat` | `https://api.x.ai/v1` | `XAI_API_KEY` |
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
- A provider with two surfaces has one preset here. To use the other, define a second provider, for example `api: anthropic-messages` with `base_url: https://api.z.ai/api/anthropic`, `https://zenmux.ai/api/anthropic`, `https://api.synthetic.new/anthropic` or `https://opencode.ai/zen`.
- `kimi-code` may refuse clients it does not recognise; azazel sends its own identity.
- `github-copilot` also accepts a GitHub token with Copilot access in `COPILOT_GITHUB_TOKEN`.

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
| `headers` | Extra headers on every request; a value may contain `${VAR}`, and a header whose variable is unset is not sent |
| `auth` | How a key is presented: `auto` (default: what the wire API prescribes, a bearer token for the OpenAI formats and Ollama, `x-api-key` for Anthropic, `x-goog-api-key` for Google), `bearer` (`Authorization: Bearer`, whatever the API) or `api-key` (an `api-key` header) |
| `discovery` | How models are listed: `auto` (default: the wire API's own list), `ollama` (`/api/tags` then `/api/show` per model), `lm-studio` (`/api/v0/models`, falling back to the OpenAI list) or `off`. `true` and `false` mean `auto` and `off` |
| `extra_body` | Members written into every request body, replacing what azazel wrote under the same name: routing (`provider = { order = ["anthropic"] }` on OpenRouter, `providerOptions = { gateway = { only = ["bedrock"] } }` on Vercel) and vendor switches (`chat_template_kwargs = { enable_thinking = true }`) |
| `region`, `profile` | `bedrock` only |
| `project`, `location` | `vertex` only |
| `oauth`, `oauth_headers` | `[providers.<id>.oauth]`: an OAuth sign-in (`flow = "pkce"` or `"device"`, `client_id`, `token_url`, `authorize_url` or `device_url`, `scopes`, …) that `azazel login <id>` then uses, and headers sent only with that sign-in |

A new provider needs a key only when it names a key source. Providers in a project's files are ignored until the project is trusted.

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
3. `api_key_command`.
4. `api_key`, a key written in the configuration.
5. The entry in `~/.azazel/auth.json`: a stored API key or a sign-in.
6. Nothing: keyless providers are called unauthenticated; any other fails with ``no credentials for <id>: set <ENV> or run `azazel login <id>` ``.

An environment key therefore wins over a stored sign-in for the same provider. `azazel doctor` shows where each provider's credential comes from, never the secret.

`auth.json` is written with mode `0600` on Unix, by temp file and rename under a lock file (`auth.json.lock`), so several azazel processes can share it. Sign-in tokens are refreshed automatically when they have less than five minutes left.

## Logging in and out

```sh
azazel login                  # lists providers and asks
azazel login groq             # asks for the key (not echoed) and stores it
azazel login anthropic        # browser sign-in with a Claude Pro or Max subscription
azazel login anthropic --key  # store an API key instead of signing in
azazel login openai-codex     # browser sign-in with a ChatGPT subscription
azazel login github-copilot   # enter a code on github.com
azazel login openrouter       # creates an OpenRouter key in the browser (--key to paste one)
azazel logout anthropic       # forget the stored key or sign-in
```

| Provider | Sign-in | Notes |
|---|---|---|
| `anthropic` | Claude Pro or Max | Browser redirects to `localhost:54545`; if it cannot reach this machine, paste the address it ends up at |
| `openai-codex` | ChatGPT Plus or Pro | The redirect address is fixed: port 1455 must be free while the login runs |
| `github-copilot` | GitHub Copilot | Device flow: enter the code shown on `github.com/login/device`. The token does not expire; revoke the app on GitHub to end it |
| `google-gemini-cli` | A Google account, as the Gemini CLI signs in | Port 8085 must be free. The Cloud project requests are made in is looked up on first use and kept with the login; accounts on a paid tier set `GOOGLE_CLOUD_PROJECT` |
| `openrouter` | OpenRouter's key flow for applications | Port 54549. What is stored is a lasting API key |
| `xai-oauth` | SuperGrok or X Premium+ | Device flow at `auth.x.ai` |

A sign-in must be finished within ten minutes; `Ctrl+C` cancels it. Providers may restrict the use of subscription accounts from third-party clients; check their terms before relying on one.

`azazel login` reads providers from the built-in list and the files in `~/.azazel` only, so a project file cannot redirect where a key is sent.

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

[providers.azure]            # optional: the address can also come from the environment
base_url = "https://<resource>.openai.azure.com"
api = "azure-openai-chat"    # default: azure-openai-responses
```

- **Bedrock** uses the AWS default credential chain and region unless `region` or `profile` is set. Models are `bedrock/<model id>`, for example `bedrock/anthropic.claude-haiku-4-5-20251001-v1:0`. Thinking is requested for Claude models only.
- **Vertex** uses Google application default credentials (`gcloud auth application-default login` or `GOOGLE_APPLICATION_CREDENTIALS`). `project` falls back to `GOOGLE_CLOUD_PROJECT`, `GCLOUD_PROJECT`, then the project of the credentials; `location` defaults to `global`. Model ids containing `claude` are sent to the Anthropic publisher (`vertex/claude-sonnet-4-5@20250929`), all others to the Google publisher.
- **Azure OpenAI** is the provider id `azure`. Its address is `AZURE_OPENAI_BASE_URL`, else `https://$AZURE_OPENAI_RESOURCE_NAME.openai.azure.com`, else `base_url`; a trailing `/openai` or `/openai/v1` is accepted. Its key comes from `AZURE_OPENAI_API_KEY` and is sent as an `api-key` header; for Entra tokens set `auth = "bearer"` and an `api_key_command` that prints the token. A model is a deployment: `azure/<deployment>`. When deployments are not named after their models, `AZURE_OPENAI_DEPLOYMENT_NAME_MAP="gpt-x=prod-x,gpt-y=prod-y"` maps them. `AZURE_OPENAI_API_VERSION` sets `api-version` (the chat API needs one and defaults to `2024-10-21`; the Responses API sends none unless it is set). Azure does not say which models are deployed, so the model picker offers the catalogue's list; add your deployments under `models:` in `models.yml` if they are named differently. The older form `api = "openai-responses"` with `base_url = "https://<resource>.openai.azure.com/openai/v1"` keeps working.
- **Amazon Bedrock through its OpenAI-compatible endpoint** is the provider `bedrock-mantle`: a Bedrock API key in `AWS_BEARER_TOKEN_BEDROCK`, region from `AWS_REGION` (default `us-east-1`). It needs no Cargo feature.

Bedrock and Vertex are Cargo features (`bedrock`, `vertex`), on by default.

## Thinking levels

Levels: `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`. The default is `off`.

| Set with | Scope |
|---|---|
| `--thinking <level>` | The lead, this run |
| `/thinking` (picker) or `/thinking <level>` | The lead, this session |
| `:level` suffix on a model spec | That model, wherever the spec is used; wins over the above |
| `thinking:` in an agent definition, `[agents.<name>] thinking` | One sub-agent |

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
| `off` | none | Only the models named in the configuration, else the catalogue's |

Models named for a provider in `models.yml` or `[models]` are always listed, whatever its endpoint reports.

See also [configuration](configuration.md) and [getting started](getting-started.md).
