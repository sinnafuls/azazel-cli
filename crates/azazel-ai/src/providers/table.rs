//! The hand-written built-in providers, one row each, sorted by id.
//!
//! A row is data only: where the endpoint is, which wire API it speaks, where its key comes
//! from and how its models are found. `${VAR}` and `${VAR:-default}` in an address are filled
//! from the environment when the providers are resolved. The providers models.dev lists beyond
//! these are in `generated.rs`; a row here replaces a generated one with the same id or for the
//! same models.dev provider, so a provider that needs a sign-in, headers or a quirk is written here.

use crate::providers::presets::{AuthStyle, Discovery};
use crate::types::WireApi;
use crate::types::WireApi::{
    AnthropicMessages, AzureOpenAiResponses, GoogleGeminiCli, GoogleGenerative, OllamaChat, OpenAiChat,
    OpenAiResponses, OpenRouter,
};

pub(super) struct Preset {
    pub id: &'static str,
    pub name: &'static str,
    pub api: WireApi,
    pub base_url: &'static str,
    /// Environment variables holding the key, tried in order.
    pub env: &'static [&'static str],
    /// A server that answers without a key (a local one, or one that only needs it for some
    /// accounts).
    pub keyless: bool,
    pub auth: AuthStyle,
    pub headers: &'static [(&'static str, &'static str)],
    pub discovery: Discovery,
    /// The provider's name on models.dev, where the catalogue snapshot takes its models from;
    /// empty when it is not listed there or its list would mislead (local servers).
    pub catalog: &'static str,
    /// Model id prefixes served on another wire API; see `ProviderConfig::routes`.
    pub routes: &'static [(&'static str, WireApi)],
}

impl Preset {
    pub(super) const fn keyless(mut self) -> Self {
        self.keyless = true;
        self
    }

    const fn auth(mut self, style: AuthStyle) -> Self {
        self.auth = style;
        self
    }

    const fn headers(mut self, headers: &'static [(&'static str, &'static str)]) -> Self {
        self.headers = headers;
        self
    }

    const fn discovery(mut self, discovery: Discovery) -> Self {
        self.discovery = discovery;
        self
    }

    pub(super) const fn catalog(mut self, name: &'static str) -> Self {
        self.catalog = name;
        self
    }

    const fn routes(mut self, routes: &'static [(&'static str, WireApi)]) -> Self {
        self.routes = routes;
        self
    }
}

/// A provider that takes a bearer key, lists its models at the wire API's usual address and is
/// called the same on models.dev.
pub(super) const fn row(
    id: &'static str,
    name: &'static str,
    api: WireApi,
    base_url: &'static str,
    env: &'static [&'static str],
) -> Preset {
    Preset {
        id,
        name,
        api,
        base_url,
        env,
        keyless: false,
        auth: AuthStyle::Auto,
        headers: &[],
        discovery: Discovery::Auto,
        catalog: id,
        routes: &[],
    }
}

/// What azazel tells a gateway that credits traffic to the application sending it.
const APP_URL: &str = "https://github.com/sinnafuls/azazel-cli";
const ATTRIBUTION: &[(&str, &str)] = &[("HTTP-Referer", APP_URL), ("X-Title", "azazel")];
/// The client identity GitHub Copilot's API serves; it refuses requests that name no editor.
const COPILOT_HEADERS: &[(&str, &str)] = &[
    ("User-Agent", "copilot/1.0.82"),
    ("Editor-Version", "copilot/1.0.82"),
    ("Copilot-Integration-Id", "copilot-developer-cli"),
    ("Openai-Intent", "conversation-agent"),
    ("X-GitHub-Api-Version", "2026-08-01"),
];
/// Where Copilot serves what its chat endpoint does not: Claude on Anthropic Messages (under
/// `/v1`), GPT-5 and later (but GPT-5 mini), Grok and MAI Code on Responses only.
const COPILOT_ROUTES: &[(&str, WireApi)] = &[
    ("claude-", AnthropicMessages),
    ("gpt-5-mini", OpenAiChat),
    ("gpt-5", OpenAiResponses),
    ("gpt-6", OpenAiResponses),
    ("gpt-7", OpenAiResponses),
    ("grok-", OpenAiResponses),
    ("mai-code-", OpenAiResponses),
];
/// OpenCode's gateways refuse a request that names no session (`400 MissingSessionID`), and ask
/// the client to say who it is.
const OPENCODE_HEADERS: &[(&str, &str)] = &[
    ("x-opencode-session", "{session_id}"),
    ("x-opencode-client", "azazel"),
];

#[rustfmt::skip]
pub(super) static PRESETS: &[Preset] = &[
    row("abliteration", "Abliteration", OpenAiResponses, "https://api.abliteration.ai/v1", &["ABLITERATION_API_KEY", "ABLIT_KEY"]).catalog("abliteration-ai"),
    row("aiand", "ai&", OpenAiChat, "https://api.aiand.com/v1", &["AIAND_API_KEY"]),
    row("aimlapi", "AIML API", OpenAiChat, "https://api.aimlapi.com/v1", &["AIMLAPI_API_KEY"]),
    row("alibaba-coding-plan", "Alibaba Coding Plan", OpenAiChat, "https://coding-intl.dashscope.aliyuncs.com/v1", &["ALIBABA_CODING_PLAN_API_KEY"]),
    row("alibaba-token-plan", "Alibaba Token Plan", OpenAiChat, "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1", &["ALIBABA_TOKEN_PLAN_API_KEY", "BAILIAN_TOKEN_PLAN_API_KEY"]),
    row("anthropic", "Anthropic", AnthropicMessages, "https://api.anthropic.com", &["ANTHROPIC_API_KEY"]),
    row("azure", "Azure OpenAI", AzureOpenAiResponses, "${AZURE_OPENAI_BASE_URL:-https://${AZURE_OPENAI_RESOURCE_NAME}.openai.azure.com}", &["AZURE_OPENAI_API_KEY"]).auth(AuthStyle::ApiKey).discovery(Discovery::Azure),
    row("baseten", "Baseten", OpenAiChat, "https://inference.baseten.co/v1", &["BASETEN_API_KEY"]),
    row("bedrock-mantle", "Amazon Bedrock Mantle", OpenAiResponses, "https://bedrock-mantle.${AWS_REGION:-us-east-1}.api.aws/v1", &["AWS_BEARER_TOKEN_BEDROCK"]).catalog(""),
    row("cerebras", "Cerebras", OpenAiChat, "https://api.cerebras.ai/v1", &["CEREBRAS_API_KEY"]),
    row("charm-hyper", "Charm Hyper", OpenAiChat, "https://hyper.charm.land/v1", &["CHARM_HYPER_API_KEY", "HYPER_API_KEY"]).catalog("hyper"),
    row("cline-pass", "ClinePass", OpenAiChat, "https://api.cline.bot/api/v1", &["CLINE_API_KEY"]),
    row("cloudflare-ai-gateway", "Cloudflare AI Gateway", OpenAiChat, "https://gateway.ai.cloudflare.com/v1/${CLOUDFLARE_ACCOUNT_ID}/${CLOUDFLARE_GATEWAY_ID}/compat", &["CLOUDFLARE_AI_GATEWAY_API_KEY", "CLOUDFLARE_API_TOKEN"]).discovery(Discovery::Off),
    row("cloudflare-workers-ai", "Cloudflare Workers AI", OpenAiChat, "https://api.cloudflare.com/client/v4/accounts/${CLOUDFLARE_ACCOUNT_ID}/ai/v1", &["CLOUDFLARE_API_TOKEN", "CLOUDFLARE_AUTH_TOKEN", "CLOUDFLARE_API_KEY"]).discovery(Discovery::Off),
    row("commandcode", "Command Code", OpenAiChat, "https://api.commandcode.ai/provider/v1", &["COMMAND_CODE_API_KEY", "COMMANDCODE_API_KEY", "CMD_API_KEY"]),
    row("coralbricks", "CoralBricks", OpenAiChat, "https://inference.coralbricks.ai/v1", &["CORAL_API_KEY", "CORALBRICKS_API_KEY"]),
    row("coreweave", "CoreWeave Serverless Inference", OpenAiChat, "https://api.inference.wandb.ai/v1", &["COREWEAVE_API_KEY", "WANDB_API_KEY"]).headers(&[("OpenAI-Project", "${COREWEAVE_PROJECT}")]).catalog("wandb"),
    row("deepinfra", "DeepInfra", OpenAiChat, "https://api.deepinfra.com/v1/openai", &["DEEPINFRA_API_KEY"]),
    row("deepseek", "DeepSeek", OpenAiChat, "https://api.deepseek.com", &["DEEPSEEK_API_KEY"]),
    row("firepass", "Fire Pass", OpenAiChat, "https://api.fireworks.ai/inference/v1", &["FIREPASS_API_KEY"]).catalog(""),
    row("fireworks", "Fireworks", OpenAiChat, "https://api.fireworks.ai/inference/v1", &["FIREWORKS_API_KEY"]).catalog("fireworks-ai"),
    row("github-copilot", "GitHub Copilot", OpenAiChat, "https://api.githubcopilot.com", &["COPILOT_GITHUB_TOKEN"]).headers(COPILOT_HEADERS).auth(AuthStyle::Bearer).routes(COPILOT_ROUTES),
    row("gmi-cloud", "GMI Cloud", OpenAiChat, "https://api.gmi-serving.com/v1", &["GMI_API_KEY"]).catalog("gmicloud"),
    row("google", "Google Gemini API", GoogleGenerative, "https://generativelanguage.googleapis.com/v1beta", &["GEMINI_API_KEY", "GOOGLE_API_KEY", "GOOGLE_GENERATIVE_AI_API_KEY"]),
    row("google-gemini-cli", "Google Cloud Code Assist (Gemini CLI)", GoogleGeminiCli, "https://cloudcode-pa.googleapis.com", &[]).discovery(Discovery::Off).catalog(""),
    row("groq", "Groq", OpenAiChat, "https://api.groq.com/openai/v1", &["GROQ_API_KEY"]),
    row("helmcode", "Helmcode", OpenAiChat, "https://api.helmcode.com/v1", &["HELMCODE_API_KEY"]),
    row("huggingface", "Hugging Face", OpenAiChat, "https://router.huggingface.co/v1", &["HUGGINGFACE_HUB_TOKEN", "HF_TOKEN"]),
    row("inception", "Inception (Mercury)", OpenAiChat, "https://api.inceptionlabs.ai/v1", &["INCEPTION_API_KEY"]),
    row("kilo", "Kilo Gateway", OpenAiChat, "https://api.kilo.ai/api/gateway", &["KILO_API_KEY"]),
    row("kimi-code", "Kimi Code", OpenAiChat, "https://api.kimi.com/coding/v1", &["KIMI_API_KEY"]).catalog("kimi-code-plan-cn"),
    row("litellm", "LiteLLM", OpenAiChat, "${LITELLM_BASE_URL:-http://localhost:4000/v1}", &["LITELLM_API_KEY"]).keyless().catalog(""),
    row("llamacpp", "llama.cpp", OpenAiChat, "http://localhost:8080/v1", &["LLAMACPP_API_KEY", "LLAMA_CPP_API_KEY"]).keyless().catalog(""),
    row("lmstudio", "LM Studio", OpenAiChat, "http://localhost:1234/v1", &["LMSTUDIO_API_KEY", "LM_STUDIO_API_KEY"]).keyless().discovery(Discovery::LmStudio).catalog(""),
    row("meta", "Meta Model API", OpenAiResponses, "https://api.meta.ai/v1", &["MODEL_API_KEY", "META_API_KEY", "META_MODEL_API_KEY"]),
    row("minimax", "MiniMax", AnthropicMessages, "https://api.minimax.io/anthropic", &["MINIMAX_API_KEY"]).discovery(Discovery::Off),
    row("minimax-code", "MiniMax Code", AnthropicMessages, "https://api.minimax.io/anthropic", &["MINIMAX_CODE_API_KEY"]).discovery(Discovery::Off).catalog("minimax-coding-plan"),
    row("minimax-code-cn", "MiniMax Code CN", AnthropicMessages, "https://api.minimax.cn/anthropic", &["MINIMAX_CODE_CN_API_KEY"]).discovery(Discovery::Off).catalog("minimax-cn-coding-plan"),
    row("mistral", "Mistral", OpenAiChat, "https://api.mistral.ai/v1", &["MISTRAL_API_KEY"]),
    row("modular", "Modular", OpenAiChat, "https://api.modular.com/v1", &["MODULAR_API_KEY"]),
    row("moonshot", "Moonshot", OpenAiChat, "${MOONSHOT_BASE_URL:-https://api.moonshot.ai/v1}", &["MOONSHOT_API_KEY"]).catalog("moonshotai"),
    row("nanogpt", "NanoGPT", OpenAiChat, "https://nano-gpt.com/api/v1", &["NANO_GPT_API_KEY"]).catalog("nano-gpt"),
    row("novita", "Novita", OpenAiChat, "https://api.novita.ai/openai/v1", &["NOVITA_API_KEY"]).catalog("novita-ai"),
    row("nvidia", "NVIDIA NIM", OpenAiChat, "https://integrate.api.nvidia.com/v1", &["NVIDIA_API_KEY"]),
    row("ollama", "Ollama", OpenAiChat, "http://localhost:11434/v1", &["OLLAMA_API_KEY"]).keyless().discovery(Discovery::Ollama).catalog(""),
    row("ollama-cloud", "Ollama Cloud", OllamaChat, "https://ollama.com", &["OLLAMA_CLOUD_API_KEY", "OLLAMA_API_KEY"]).discovery(Discovery::Ollama),
    row("openai", "OpenAI", OpenAiResponses, "https://api.openai.com/v1", &["OPENAI_API_KEY"]),
    row("openai-codex", "ChatGPT (Codex)", OpenAiResponses, "https://chatgpt.com/backend-api/codex", &[]).catalog(""),
    row("opencode-go", "OpenCode Go", OpenAiChat, "https://opencode.ai/inference/go/openai/v1", &["OPENCODE_API_KEY"]).headers(OPENCODE_HEADERS),
    row("opencode-zen", "OpenCode Zen", OpenAiChat, "https://opencode.ai/inference/openai/v1", &["OPENCODE_API_KEY"]).headers(OPENCODE_HEADERS).catalog("opencode"),
    row("openrouter", "OpenRouter", OpenRouter, "https://openrouter.ai/api/v1", &["OPENROUTER_API_KEY"]).headers(ATTRIBUTION),
    row("otari", "Otari", OpenAiChat, "${OTARI_BASE_URL:-https://api.otari.ai/api/v1}", &["OTARI_API_KEY"]).catalog(""),
    row("qianfan", "Qianfan", OpenAiChat, "https://qianfan.baidubce.com/v2", &["QIANFAN_API_KEY"]).catalog(""),
    row("qwen-portal", "Qwen Portal", OpenAiChat, "https://portal.qwen.ai/v1", &["QWEN_OAUTH_TOKEN", "QWEN_PORTAL_API_KEY"]).discovery(Discovery::Off).catalog(""),
    row("sakana", "Sakana AI", OpenAiResponses, "https://api.sakana.ai/v1", &["SAKANA_API_KEY", "FUGU_API_KEY"]),
    row("sambanova", "SambaNova", OpenAiChat, "https://api.sambanova.ai/v1", &["SAMBANOVA_API_KEY"]),
    row("scaleway", "Scaleway", OpenAiChat, "https://api.scaleway.ai/v1", &["SCW_SECRET_KEY", "SCW_API_KEY", "SCALEWAY_API_KEY"]),
    row("siliconflow", "SiliconFlow", OpenAiChat, "https://api.siliconflow.com/v1", &["SILICONFLOW_API_KEY"]),
    row("siliconflow-cn", "SiliconFlow CN", OpenAiChat, "https://api.siliconflow.cn/v1", &["SILICONFLOW_CN_API_KEY"]),
    row("singularityapi-dev", "SingularityAPI (pay as you go)", OpenAiChat, "https://api.singularityapi.dev/v1", &["SINGULARITYAPI_DEV_API_KEY"]).catalog(""),
    row("singularityapi-tech", "SingularityAPI (reserved lanes)", OpenAiChat, "https://api.singularityapi.tech/v1", &["SINGULARITYAPI_TECH_API_KEY"]).catalog(""),
    row("snowflake", "Snowflake Cortex", OpenAiChat, "https://${SNOWFLAKE_ACCOUNT}.snowflakecomputing.com/api/v2/cortex/v1", &["SNOWFLAKE_PAT", "SNOWFLAKE_CORTEX_PAT"]).discovery(Discovery::Off).catalog("snowflake-cortex"),
    row("stepfun", "StepFun", OpenAiChat, "https://api.stepfun.ai/v1", &["STEPFUN_API_KEY"]).catalog("stepfun-ai"),
    row("synthetic", "Synthetic", OpenAiChat, "https://api.synthetic.new/openai/v1", &["SYNTHETIC_API_KEY"]),
    row("together", "Together", OpenAiChat, "https://api.together.xyz/v1", &["TOGETHER_API_KEY"]).catalog("togetherai"),
    row("umans", "Umans AI Coding Plan", OpenAiChat, "https://api.code.umans.ai/v1", &["UMANS_AI_CODING_PLAN_API_KEY"]).catalog("umans-ai-coding-plan"),
    row("venice", "Venice", OpenAiChat, "https://api.venice.ai/api/v1", &["VENICE_API_KEY"]),
    row("vercel-ai-gateway", "Vercel AI Gateway", OpenAiChat, "https://ai-gateway.vercel.sh/v1", &["AI_GATEWAY_API_KEY", "VERCEL_AI_GATEWAY_API_KEY"]).headers(ATTRIBUTION).catalog("vercel"),
    row("vllm", "vLLM", OpenAiChat, "${VLLM_BASE_URL:-http://localhost:8000/v1}", &["VLLM_API_KEY"]).keyless().catalog(""),
    row("wafer-serverless", "Wafer Serverless", OpenAiChat, "https://pass.wafer.ai/v1", &["WAFER_SERVERLESS_API_KEY", "WAFER_API_KEY"]).catalog("wafer.ai"),
    row("xai", "xAI", OpenAiResponses, "https://api.x.ai/v1", &["XAI_API_KEY"]),
    row("xai-oauth", "xAI Grok (SuperGrok)", OpenAiResponses, "https://api.x.ai/v1", &["XAI_OAUTH_TOKEN"]).catalog("xai"),
    row("xiaomi", "Xiaomi MiMo", OpenAiChat, "https://api.xiaomimimo.com/v1", &["XIAOMI_API_KEY"]),
    row("xiaomi-token-plan-ams", "Xiaomi Token Plan (AMS)", OpenAiChat, "https://token-plan-ams.xiaomimimo.com/v1", &["XIAOMI_TOKEN_PLAN_AMS_API_KEY"]),
    row("xiaomi-token-plan-cn", "Xiaomi Token Plan (CN)", OpenAiChat, "https://token-plan-cn.xiaomimimo.com/v1", &["XIAOMI_TOKEN_PLAN_CN_API_KEY"]),
    row("xiaomi-token-plan-sgp", "Xiaomi Token Plan (SGP)", OpenAiChat, "https://token-plan-sgp.xiaomimimo.com/v1", &["XIAOMI_TOKEN_PLAN_SGP_API_KEY"]),
    row("yolo-auto", "Yolo-Auto", OpenAiChat, "https://yolo-auto.com/v1", &["YOLO_AUTO_API_KEY"]).catalog(""),
    row("zai", "Z.AI (GLM Coding Plan)", OpenAiChat, "https://api.z.ai/api/coding/paas/v4", &["ZAI_API_KEY"]).catalog("zai-coding-plan"),
    row("zenmux", "ZenMux", OpenAiChat, "https://zenmux.ai/api/v1", &["ZENMUX_API_KEY"]),
    row("zhipu-coding-plan", "Zhipu Coding Plan", OpenAiChat, "https://open.bigmodel.cn/api/coding/paas/v4", &["ZHIPU_API_KEY"]).catalog("zhipuai-coding-plan"),
];
