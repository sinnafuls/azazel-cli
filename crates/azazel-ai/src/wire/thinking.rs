//! Maps the unified thinking level onto what each API accepts.

use crate::catalog::ModelInfo;
use crate::types::{ThinkingLevel, WireApi};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingParam {
    /// Send no thinking parameters at all.
    Off,
    /// Reasoning token budget.
    Budget(u32),
    /// Named reasoning effort.
    Effort(&'static str),
}

/// `Off` when the model does not reason or the level is `Off`. Budget APIs get a token budget that
/// always leaves at least 1024 output tokens; effort APIs get the nearest effort name they know.
pub fn thinking_param(api: WireApi, model: &ModelInfo, level: ThinkingLevel) -> ThinkingParam {
    if !model.reasoning || level == ThinkingLevel::Off {
        return ThinkingParam::Off;
    }
    match api {
        WireApi::OpenAiResponses
        | WireApi::OpenAiChat
        | WireApi::AzureOpenAiResponses
        | WireApi::AzureOpenAiChat => ThinkingParam::Effort(match level {
            ThinkingLevel::Off | ThinkingLevel::Minimal => "minimal",
            ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => "high",
        }),
        // OpenRouter translates its own effort names for whichever model answers.
        WireApi::OpenRouter => ThinkingParam::Effort(match level {
            ThinkingLevel::Off | ThinkingLevel::Minimal => "minimal",
            ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::High => "high",
            ThinkingLevel::Xhigh | ThinkingLevel::Max => "xhigh",
        }),
        WireApi::OllamaChat => ThinkingParam::Effort(match level {
            ThinkingLevel::Off | ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::High | ThinkingLevel::Xhigh => "high",
            ThinkingLevel::Max => "max",
        }),
        WireApi::AnthropicMessages
        | WireApi::GoogleGenerative
        | WireApi::GoogleGeminiCli
        | WireApi::BedrockConverse => {
            let ceiling = model.max_output.saturating_sub(1024).max(1024);
            let budget = match level {
                ThinkingLevel::Off | ThinkingLevel::Minimal => 1024,
                ThinkingLevel::Low => 4096,
                ThinkingLevel::Medium => 10_000,
                ThinkingLevel::High => 24_000,
                ThinkingLevel::Xhigh => 48_000,
                ThinkingLevel::Max => ceiling,
            };
            ThinkingParam::Budget(budget.min(ceiling))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(reasoning: bool, max_output: u32) -> ModelInfo {
        ModelInfo {
            reasoning,
            max_output,
            ..ModelInfo::unknown("mock", "m")
        }
    }

    #[test]
    fn budget_levels_and_ceiling() {
        let m = model(true, 64_000);
        let budget = |level| thinking_param(WireApi::AnthropicMessages, &m, level);
        assert_eq!(budget(ThinkingLevel::Minimal), ThinkingParam::Budget(1024));
        assert_eq!(budget(ThinkingLevel::Medium), ThinkingParam::Budget(10_000));
        assert_eq!(budget(ThinkingLevel::Xhigh), ThinkingParam::Budget(48_000));
        assert_eq!(budget(ThinkingLevel::Max), ThinkingParam::Budget(62_976));
        // A small output limit caps every level.
        assert_eq!(
            thinking_param(WireApi::GoogleGenerative, &model(true, 8192), ThinkingLevel::High),
            ThinkingParam::Budget(7168)
        );
    }

    #[test]
    fn effort_apis_use_the_names_they_know() {
        let m = model(true, 64_000);
        assert_eq!(
            thinking_param(WireApi::OpenAiChat, &m, ThinkingLevel::Low),
            ThinkingParam::Effort("low")
        );
        assert_eq!(
            thinking_param(WireApi::OpenAiResponses, &m, ThinkingLevel::Max),
            ThinkingParam::Effort("high")
        );
        assert_eq!(
            thinking_param(WireApi::AzureOpenAiChat, &m, ThinkingLevel::Xhigh),
            ThinkingParam::Effort("high")
        );
        assert_eq!(
            thinking_param(WireApi::OpenRouter, &m, ThinkingLevel::Max),
            ThinkingParam::Effort("xhigh")
        );
        assert_eq!(
            thinking_param(WireApi::OllamaChat, &m, ThinkingLevel::Minimal),
            ThinkingParam::Effort("low")
        );
        assert_eq!(
            thinking_param(WireApi::OllamaChat, &m, ThinkingLevel::Max),
            ThinkingParam::Effort("max")
        );
    }
}
