//! The profiles the program ships. Each is one provider's models at four tiers; a profile of the
//! same name in a config file replaces it whole.

use std::collections::BTreeMap;

use azazel_ai::ModelSpec;

use super::{Profile, ProfileSource};

const OPUS: &str = "anthropic/claude-opus-5-5";
const FABLE: &str = "anthropic/claude-fable-5-2";
const SONNET: &str = "anthropic/claude-sonnet-5-5";
const HAIKU: &str = "anthropic/claude-haiku-5-5";

// The subscription backend lists its own models, so the catalogue has these only under `openai`.
const GPT_FABLE: &str = "openai-codex/gpt-6-astra";
const GPT_OPUS: &str = "openai-codex/gpt-6.1-sol";
const GPT_SONNET: &str = "openai-codex/gpt-6.1-terra";
const GPT_HAIKU: &str = "openai-codex/gpt-6-luna";

/// The models of a profile by what they are for: `default` is the lead's, `slow` the one for
/// planning and review, `work` the coders, testers and scouts, `smol` the quick structured tasks.
struct Tiers {
    default: &'static str,
    slow: &'static str,
    work: &'static str,
    smol: &'static str,
}

fn profile(name: &str, tiers: Tiers, extra: &[(&str, &str)]) -> Profile {
    let mut roles: BTreeMap<String, &str> = BTreeMap::new();
    roles.insert("default".to_string(), tiers.default);
    for role in ["slow", "reviewer"] {
        roles.insert(role.to_string(), tiers.slow);
    }
    for role in ["coder", "tester", "scout", "vision"] {
        roles.insert(role.to_string(), tiers.work);
    }
    roles.insert("smol".to_string(), tiers.smol);
    roles.extend(extra.iter().map(|(role, model)| (role.to_string(), *model)));
    Profile {
        name: name.to_string(),
        source: ProfileSource::Builtin,
        roles: roles
            .into_iter()
            .filter_map(|(role, model)| Some((role, vec![model.parse::<ModelSpec>().ok()?])))
            .collect(),
    }
}

/// The built-in profiles, in the order they are listed.
pub fn all() -> Vec<Profile> {
    vec![
        profile(
            "claude",
            Tiers {
                default: OPUS,
                slow: FABLE,
                work: SONNET,
                smol: HAIKU,
            },
            &[],
        ),
        profile(
            "gpt",
            Tiers {
                default: GPT_OPUS,
                slow: GPT_FABLE,
                work: GPT_SONNET,
                smol: GPT_HAIKU,
            },
            &[],
        ),
        profile(
            "cheap",
            Tiers {
                default: SONNET,
                slow: SONNET,
                work: HAIKU,
                smol: HAIKU,
            },
            &[],
        ),
        profile(
            "thorough",
            Tiers {
                default: FABLE,
                slow: FABLE,
                work: OPUS,
                smol: SONNET,
            },
            &[],
        ),
        profile(
            "frontend",
            Tiers {
                default: OPUS,
                slow: FABLE,
                work: SONNET,
                smol: HAIKU,
            },
            &[("designer", FABLE)],
        ),
        profile(
            "research",
            Tiers {
                default: OPUS,
                slow: FABLE,
                work: SONNET,
                smol: HAIKU,
            },
            &[("researcher", OPUS), ("scout", OPUS)],
        ),
    ]
}
