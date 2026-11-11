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
/// always leaves at least 1024 output tokens; effort APIs get `minimal|low|medium|high`.
pub fn thinking_param(api: WireApi, model: &ModelInfo, level: ThinkingLevel) -> ThinkingParam {
    if !model.reasoning || level == ThinkingLevel::Off {
        return ThinkingParam::Off;
    }
    match api {
        WireApi::OpenAiResponses | WireApi::OpenAiChat => ThinkingParam::Effort(match level {
            ThinkingLevel::Off | ThinkingLevel::Minimal => "minimal",
            ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => "high",
        }),
        WireApi::AnthropicMessages | WireApi::GoogleGenerative | WireApi::BedrockConverse => {
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
    fn off_when_model_cannot_reason_or_level_is_off() {
        assert_eq!(
            thinking_param(
                WireApi::AnthropicMessages,
                &model(false, 64_000),
                ThinkingLevel::High
            ),
            ThinkingParam::Off
        );
        assert_eq!(
            thinking_param(WireApi::OpenAiResponses, &model(true, 64_000), ThinkingLevel::Off),
            ThinkingParam::Off
        );
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
    fn effort_apis_cap_at_high() {
        let m = model(true, 64_000);
        assert_eq!(
            thinking_param(WireApi::OpenAiChat, &m, ThinkingLevel::Low),
            ThinkingParam::Effort("low")
        );
        assert_eq!(
            thinking_param(WireApi::OpenAiResponses, &m, ThinkingLevel::Max),
            ThinkingParam::Effort("high")
        );
    }
}
