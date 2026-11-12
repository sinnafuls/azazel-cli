//! `models.yml`: providers and their models, defined by the user in YAML.
//!
//! ```yaml
//! providers:
//!   gateway:                      # a new provider needs `api` and `base_url` in some layer
//!     name: Team gateway
//!     api: openai-chat
//!     base_url: https://llm.example.com/v1
//!     api_key_env: GATEWAY_API_KEY
//!     headers: { X-Team: platform }
//!     discovery: off
//!     models:
//!       - id: big-model
//!         name: Big Model
//!         context_window: 200000
//!         max_output: 32000
//!         vision: true
//!         thinking_levels: [low, medium, high]
//!         cost: { input: 3.0, output: 15.0 }
//!   ollama:                       # a built-in provider: only what is named changes
//!     base_url: http://192.168.1.20:11434/v1
//! ```
//!
//! A provider entry takes the keys of a `[providers.<id>]` table of `config.toml`
//! ([`ProviderSettings`]) plus `models`; a model entry takes `id` and the keys of a
//! `[models."<provider>/<id>"]` table ([`ModelOverride`]).

use std::collections::BTreeMap;

use serde::de::DeserializeOwned;
use serde_yaml_ng::{Mapping, Value};

use crate::catalog::ModelOverride;
use crate::providers::presets::ProviderSettings;

/// The name of the file in `~/.azazel` and in a project's `.azazel`.
pub const FILE_NAME: &str = "models.yml";

/// What one `models.yml` defines.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelsFile {
    pub providers: BTreeMap<String, ProviderSettings>,
    /// Keyed by `provider/id`, like the `[models]` table of `config.toml`.
    pub models: BTreeMap<String, ModelOverride>,
}

impl ModelsFile {
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty() && self.models.is_empty()
    }
}

/// Reads the text of a `models.yml`. An error names the key it is about
/// (`providers.gateway.models[0].cost`), or the line for text that is not YAML.
pub fn parse(text: &str) -> Result<ModelsFile, String> {
    let root: Value = serde_yaml_ng::from_str(text).map_err(|e| e.to_string())?;
    let mut file = ModelsFile::default();
    let root = match root {
        Value::Null => return Ok(file),
        Value::Mapping(root) => root,
        _ => return Err("expected a mapping with the key `providers`".to_string()),
    };
    for (key, value) in &root {
        match key.as_str() {
            Some("providers") => {}
            _ => return Err(format!("unknown key `{}`, expected `providers`", scalar(key))),
        }
        let providers = match value {
            Value::Null => continue,
            Value::Mapping(providers) => providers,
            _ => return Err("providers: expected a mapping of provider id to its settings".to_string()),
        };
        for (id, entry) in providers {
            let id = scalar(id);
            if id.is_empty() || id.contains('/') {
                return Err(format!(
                    "providers: `{id}` is not a provider id (it must be non-empty and have no `/`)"
                ));
            }
            provider(&mut file, &id, entry)?;
        }
    }
    Ok(file)
}

/// A key as it is written in messages.
fn scalar(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => serde_yaml_ng::to_string(other)
            .map(|text| text.trim().to_string())
            .unwrap_or_default(),
    }
}

/// `entries` as `T`. Each key is read on its own first, so a wrong value is reported under the
/// key that holds it.
fn typed<T: DeserializeOwned>(entries: &Mapping, at: &str) -> Result<T, String> {
    for (key, value) in entries {
        let mut single = Mapping::new();
        single.insert(key.clone(), value.clone());
        serde_yaml_ng::from_value::<T>(Value::Mapping(single)).map_err(|e| {
            let message = e.to_string();
            // An unknown key is already named by the message.
            match message.starts_with("unknown field") {
                true => format!("{at}: {message}"),
                false => format!("{at}.{}: {message}", scalar(key)),
            }
        })?;
    }
    serde_yaml_ng::from_value(Value::Mapping(entries.clone())).map_err(|e| format!("{at}: {e}"))
}

fn provider(file: &mut ModelsFile, id: &str, entry: &Value) -> Result<(), String> {
    let at = format!("providers.{id}");
    let mut entries = match entry {
        Value::Null => Mapping::new(),
        Value::Mapping(entries) => entries.clone(),
        _ => return Err(format!("{at}: expected a mapping of settings")),
    };
    let models = entries.remove("models");
    let settings: ProviderSettings = typed(&entries, &at)?;
    if let Some(oauth) = &settings.oauth {
        oauth.check().map_err(|why| format!("{at}.oauth: {why}"))?;
    }
    // An entry that only lists models describes a provider defined somewhere else. Whether a
    // new provider is complete is checked once every layer has been merged: a project's file
    // may adjust a provider the home file defines.
    if settings != ProviderSettings::default() {
        file.providers.insert(id.to_string(), settings);
    }

    let models = match models {
        None | Some(Value::Null) => return Ok(()),
        Some(Value::Sequence(models)) => models,
        Some(_) => return Err(format!("{at}.models: expected a list of models")),
    };
    for (index, model) in models.iter().enumerate() {
        let at = format!("{at}.models[{index}]");
        let Value::Mapping(model) = model else {
            return Err(format!("{at}: expected a mapping with at least `id`"));
        };
        let mut fields = model.clone();
        let model_id = match fields.remove("id") {
            Some(Value::String(model_id)) if !model_id.trim().is_empty() => model_id.trim().to_string(),
            Some(_) => return Err(format!("{at}.id: expected a non-empty string")),
            None => return Err(format!("{at}: `id` is missing")),
        };
        let described: ModelOverride = typed(&fields, &at)?;
        if described.context_window == Some(0) {
            return Err(format!("{at}.context_window: must be greater than 0"));
        }
        if described.max_output == Some(0) {
            return Err(format!("{at}.max_output: must be greater than 0"));
        }
        if let (Some(window), Some(output)) = (described.context_window, described.max_output)
            && u64::from(output) > window
        {
            return Err(format!(
                "{at}.max_output: {output} is more than the context window ({window})"
            ));
        }
        let key = format!("{id}/{model_id}");
        if file.models.insert(key, described).is_some() {
            return Err(format!("{at}.id: `{model_id}` is listed twice"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Cost;
    use crate::providers::presets::{AuthStyle, Discovery};
    use crate::types::{ThinkingLevel, WireApi};

    const EXAMPLE: &str = r#"
providers:
  gateway:
    name: Team gateway
    api: openai-chat
    base_url: https://llm.example.com/v1
    api_key_env: GATEWAY_API_KEY
    auth: api-key
    headers: { X-Team: platform }
    discovery: off
    extra_body:
      provider: { order: [a, b] }
    models:
      - id: big-model
        name: Big Model
        context_window: 200000
        max_output: 32000
        vision: true
        thinking_levels: [low, medium, high]
        cost: { input: 3.0, output: 15.0, cache_read: 0.3 }
      - id: small-model
  ollama:
    base_url: http://192.168.1.20:11434/v1
  bedrock:
    models:
      - id: vendor.model-v1:0
        reasoning: true
"#;

    #[test]
    fn reads_providers_and_their_models() {
        let file = parse(EXAMPLE).unwrap();
        assert_eq!(
            file.providers.keys().collect::<Vec<_>>(),
            ["gateway", "ollama"],
            "an entry that only lists models configures no provider"
        );
        let gateway = &file.providers["gateway"];
        assert_eq!(gateway.name.as_deref(), Some("Team gateway"));
        assert_eq!(gateway.api, Some(WireApi::OpenAiChat));
        assert_eq!(gateway.base_url.as_deref(), Some("https://llm.example.com/v1"));
        assert_eq!(gateway.api_key_env.as_deref(), Some("GATEWAY_API_KEY"));
        assert_eq!(gateway.auth, Some(AuthStyle::ApiKey));
        assert_eq!(gateway.headers["X-Team"], "platform");
        assert_eq!(gateway.discovery, Some(Discovery::Off));
        assert_eq!(gateway.extra_body["provider"]["order"][0], "a");
        assert_eq!(
            file.providers["ollama"].base_url.as_deref(),
            Some("http://192.168.1.20:11434/v1")
        );

        assert_eq!(
            file.models.keys().collect::<Vec<_>>(),
            [
                "bedrock/vendor.model-v1:0",
                "gateway/big-model",
                "gateway/small-model"
            ]
        );
        let big = &file.models["gateway/big-model"];
        assert_eq!(big.name.as_deref(), Some("Big Model"));
        assert_eq!(
            (big.context_window, big.max_output),
            (Some(200_000), Some(32_000))
        );
        assert_eq!(big.vision, Some(true));
        assert_eq!(
            big.thinking_levels.as_deref(),
            Some(&[ThinkingLevel::Low, ThinkingLevel::Medium, ThinkingLevel::High][..])
        );
        assert_eq!(
            big.cost,
            Some(Cost {
                input: 3.0,
                output: 15.0,
                cache_read: 0.3,
                cache_write: 0.0
            })
        );
        assert_eq!(file.models["gateway/small-model"], ModelOverride::default());
    }

    #[test]
    fn an_empty_file_defines_nothing() {
        assert!(parse("").unwrap().is_empty());
        assert!(parse("providers:\n").unwrap().is_empty());
        assert!(parse("# only a comment\n").unwrap().is_empty());
    }

    #[test]
    fn a_mistake_is_reported_under_its_key() {
        let error = |text: &str| parse(text).unwrap_err();
        let has = |text: &str, parts: &[&str]| {
            let message = error(text);
            assert!(
                parts.iter().all(|part| message.contains(part)),
                "`{message}` should mention {parts:?}"
            );
        };
        has("models: []\n", &["unknown key `models`"]);
        has(
            "providers:\n  gw:\n    api: openai-chat\n    base_url: http://x\n    apikey: k\n",
            &["providers.gw", "unknown field `apikey`"],
        );
        has(
            "providers:\n  gw:\n    api: smoke-signals\n    base_url: http://x\n",
            &["providers.gw.api", "smoke-signals"],
        );
        has("providers:\n  a/b:\n    api: openai-chat\n", &["`a/b`"]);
        has(
            "providers:\n  openai:\n    models:\n      - name: nameless\n",
            &["providers.openai.models[0]", "`id` is missing"],
        );
        has(
            "providers:\n  openai:\n    models:\n      - id: m\n        context_window: lots\n",
            &["providers.openai.models[0].context_window"],
        );
        has(
            "providers:\n  openai:\n    models:\n      - id: m\n        cost: { input: 1 }\n",
            &["providers.openai.models[0].cost", "output"],
        );
        has(
            "providers:\n  openai:\n    models:\n      - id: m\n        context_window: 1000\n        max_output: 2000\n",
            &["providers.openai.models[0].max_output"],
        );
        has(
            "providers:\n  openai:\n    models:\n      - id: m\n      - id: m\n",
            &["providers.openai.models[1].id", "twice"],
        );
        has(
            "providers:\n  openai:\n    models:\n      - id: m\n        thinking_levels: [extreme]\n",
            &["providers.openai.models[0].thinking_levels", "extreme"],
        );
        // Text that is not YAML at all is reported with where it breaks.
        assert!(error("providers: [unclosed\n").contains("line"));
    }
}
