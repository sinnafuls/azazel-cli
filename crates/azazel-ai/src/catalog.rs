//! Model metadata: context window, output limit, capabilities and prices.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use serde::{Deserialize, Serialize};

/// USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cost {
    pub input: f64,
    pub output: f64,
    #[serde(default)]
    pub cache_read: f64,
    #[serde(default)]
    pub cache_write: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub provider: String,
    pub id: String,
    pub context_window: u64,
    pub max_output: u32,
    #[serde(default)]
    pub vision: bool,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
}

impl ModelInfo {
    /// Conservative metadata for a model the catalogue does not know.
    pub fn unknown(provider: &str, id: &str) -> Self {
        ModelInfo {
            provider: provider.to_string(),
            id: id.to_string(),
            context_window: 128_000,
            max_output: 8192,
            vision: false,
            reasoning: false,
            cost: None,
        }
    }

    pub fn key(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }
}

/// A `[models."provider/id"]` config table.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelOverride {
    pub context_window: Option<u64>,
    pub max_output: Option<u32>,
    pub vision: Option<bool>,
    pub reasoning: Option<bool>,
    pub cost: Option<Cost>,
}

/// On-disk shape of the embedded snapshot and of `~/.azazel/cache/models.json`.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CatalogFile {
    pub models: Vec<ModelInfo>,
}

#[derive(Debug, Clone, Default)]
pub struct Catalog {
    models: HashMap<String, ModelInfo>,
}

impl Catalog {
    /// The snapshot compiled into the binary.
    pub fn embedded() -> Self {
        let mut catalog = Catalog::default();
        match serde_json::from_str::<CatalogFile>(include_str!("../catalog/models.json")) {
            Ok(file) => catalog.merge(file.models),
            Err(e) => tracing::warn!("embedded model catalogue is invalid: {e}"),
        }
        catalog
    }

    /// Embedded snapshot, then the cache file when it exists and parses, then config overrides.
    pub fn load(cache_file: Option<&Path>, overrides: &BTreeMap<String, ModelOverride>) -> Self {
        let mut catalog = Catalog::embedded();
        if let Some(path) = cache_file
            && let Ok(text) = std::fs::read_to_string(path)
        {
            match serde_json::from_str::<CatalogFile>(&text) {
                Ok(file) => catalog.merge(file.models),
                Err(e) => tracing::warn!("ignoring {}: {e}", path.display()),
            }
        }
        catalog.apply_overrides(overrides);
        catalog
    }

    /// Later entries replace earlier ones with the same `provider/id`.
    pub fn merge(&mut self, models: impl IntoIterator<Item = ModelInfo>) {
        for model in models {
            self.models.insert(model.key(), model);
        }
    }

    /// Keys are `provider/id`; an override for an unknown model starts from [`ModelInfo::unknown`].
    pub fn apply_overrides(&mut self, overrides: &BTreeMap<String, ModelOverride>) {
        for (key, o) in overrides {
            let Some((provider, id)) = key.split_once('/') else {
                tracing::warn!("ignoring model override `{key}`: expected `provider/id`");
                continue;
            };
            let model = self
                .models
                .entry(key.clone())
                .or_insert_with(|| ModelInfo::unknown(provider, id));
            if let Some(v) = o.context_window {
                model.context_window = v;
            }
            if let Some(v) = o.max_output {
                model.max_output = v;
            }
            if let Some(v) = o.vision {
                model.vision = v;
            }
            if let Some(v) = o.reasoning {
                model.reasoning = v;
            }
            if let Some(v) = o.cost {
                model.cost = Some(v);
            }
        }
    }

    pub fn get(&self, provider: &str, id: &str) -> ModelInfo {
        self.models
            .get(&format!("{provider}/{id}"))
            .cloned()
            .unwrap_or_else(|| ModelInfo::unknown(provider, id))
    }

    pub fn contains(&self, provider: &str, id: &str) -> bool {
        self.models.contains_key(&format!("{provider}/{id}"))
    }

    /// Models of one provider, sorted by id.
    pub fn for_provider(&self, provider: &str) -> Vec<&ModelInfo> {
        let mut models: Vec<&ModelInfo> = self.models.values().filter(|m| m.provider == provider).collect();
        models.sort_by(|a, b| a.id.cmp(&b.id));
        models
    }

    pub fn len(&self) -> usize {
        self.models.len()
    }

    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }
}

impl Catalog {
    /// Records `models` as what `provider` offers in `cache`, leaving every other provider's
    /// entries alone. A provider whose list only a login can fetch (a subscription backend) is
    /// written here after it answered; the next session starts from what it said.
    pub fn refresh_cache(cache: &Path, provider: &str, models: &[ModelInfo]) -> std::io::Result<()> {
        if let Some(dir) = cache.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut file = std::fs::read_to_string(cache)
            .ok()
            .and_then(|text| serde_json::from_str::<CatalogFile>(&text).ok())
            .unwrap_or_default();
        file.models.retain(|model| model.provider != provider);
        file.models.extend(models.iter().cloned());
        let text = serde_json::to_string_pretty(&file)?;
        std::fs::write(cache, text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_snapshot_parses() {
        serde_json::from_str::<CatalogFile>(include_str!("../catalog/models.json")).unwrap();
    }

    #[test]
    fn unknown_model_gets_conservative_defaults() {
        let m = Catalog::default().get("mock", "nope");
        assert_eq!(
            (m.context_window, m.max_output, m.vision, m.reasoning, m.cost),
            (128_000, 8192, false, false, None)
        );
    }

    #[test]
    fn later_layers_win_and_overrides_apply_per_field() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("models.json");
        let mut base = ModelInfo::unknown("mock", "a");
        base.context_window = 1000;
        base.vision = true;
        std::fs::write(
            &cache,
            serde_json::to_string(&CatalogFile { models: vec![base] }).unwrap(),
        )
        .unwrap();

        let mut overrides = BTreeMap::new();
        overrides.insert(
            "mock/a".to_string(),
            ModelOverride {
                max_output: Some(77),
                ..Default::default()
            },
        );
        overrides.insert(
            "mock/new".to_string(),
            ModelOverride {
                reasoning: Some(true),
                ..Default::default()
            },
        );

        let catalog = Catalog::load(Some(&cache), &overrides);
        let a = catalog.get("mock", "a");
        assert_eq!((a.context_window, a.max_output, a.vision), (1000, 77, true));
        let new = catalog.get("mock", "new");
        assert_eq!((new.context_window, new.reasoning), (128_000, true));
    }
}
