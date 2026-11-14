//! Model metadata: context window, output limit, capabilities and prices.

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::Path;

use flate2::read::DeflateDecoder;
use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use serde::{Deserialize, Serialize};

use crate::types::{ModelRef, ThinkingLevel};

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
    /// What the model is called in lists, when that is more than its id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub context_window: u64,
    pub max_output: u32,
    #[serde(default)]
    pub vision: bool,
    #[serde(default)]
    pub reasoning: bool,
    /// The thinking levels the model accepts; empty means every level.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thinking_levels: Vec<ThinkingLevel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
}

impl ModelInfo {
    /// Conservative metadata for a model the catalogue does not know.
    pub fn unknown(provider: &str, id: &str) -> Self {
        ModelInfo {
            provider: provider.to_string(),
            id: id.to_string(),
            name: None,
            context_window: 128_000,
            max_output: 8192,
            vision: false,
            reasoning: false,
            thinking_levels: Vec::new(),
            cost: None,
        }
    }

    pub fn key(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }

    /// The level a request for `wanted` is made at (see [`ThinkingLevel::nearest`]).
    pub fn thinking_level(&self, wanted: ThinkingLevel) -> ThinkingLevel {
        wanted.nearest(&self.thinking_levels)
    }
}

/// A `[models."provider/id"]` config table, or one entry of a provider's `models:` list in
/// `models.yml`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
    /// The thinking levels the model accepts. Naming any makes the model a reasoning one unless
    /// `reasoning` says otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_levels: Option<Vec<ThinkingLevel>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
}

/// On-disk shape of the embedded snapshot and of `~/.azazel/cache/models.json`.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CatalogFile {
    pub models: Vec<ModelInfo>,
}

/// `catalog/models.json` as the build script deflated it. It is the largest piece of data in the
/// binary, and a fifth of its size this way.
static SNAPSHOT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/models.json.deflate"));

/// The JSON of the snapshot compiled into the binary.
fn snapshot() -> std::io::Result<Vec<u8>> {
    let mut json = Vec::with_capacity(SNAPSHOT.len() * 8);
    DeflateDecoder::new(SNAPSHOT).read_to_end(&mut json)?;
    Ok(json)
}

#[derive(Debug, Clone, Default)]
pub struct Catalog {
    models: HashMap<String, ModelInfo>,
}

impl Catalog {
    /// The snapshot compiled into the binary, inflated and read when it is asked for.
    pub fn embedded() -> Self {
        let mut catalog = Catalog::default();
        let file = snapshot().and_then(|json| Ok(serde_json::from_slice::<CatalogFile>(&json)?));
        match file {
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

    /// Later entries replace earlier ones with the same `provider/id`, except for the thinking
    /// levels: a provider's own model list never names them, so an entry that knows none keeps
    /// the levels the one it replaces knew.
    pub fn merge(&mut self, models: impl IntoIterator<Item = ModelInfo>) {
        for mut model in models {
            if model.thinking_levels.is_empty()
                && let Some(known) = self.models.get(&model.key())
            {
                model.thinking_levels = known.thinking_levels.clone();
            }
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
            if let Some(v) = &o.name {
                model.name = Some(v.clone());
            }
            if let Some(v) = o.context_window {
                model.context_window = v;
            }
            if let Some(v) = o.max_output {
                model.max_output = v;
            }
            if let Some(v) = o.vision {
                model.vision = v;
            }
            if let Some(levels) = &o.thinking_levels {
                model.thinking_levels = levels.clone();
                model.reasoning = levels.iter().any(|level| *level != ThinkingLevel::Off);
            }
            if let Some(v) = o.reasoning {
                model.reasoning = v;
            }
            if let Some(v) = o.cost {
                model.cost = Some(v);
            }
        }
    }

    /// The models the configuration names for `provider`, as the catalogue now describes them:
    /// what a provider lists besides whatever its own endpoint reports.
    pub fn declared(&self, overrides: &BTreeMap<String, ModelOverride>, provider: &str) -> Vec<ModelInfo> {
        overrides
            .keys()
            .filter_map(|key| key.split_once('/'))
            .filter(|(named, _)| *named == provider)
            .map(|(_, id)| self.get(provider, id))
            .collect()
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

    /// The model a short or partial name means, among the models of `providers`: an exact id
    /// first, else the single best fuzzy match of `provider/id`. A trailing `:level` sets the
    /// thinking level. `Err` holds the best candidates when several match equally well, and is
    /// empty when nothing matches.
    pub fn find(&self, query: &str, providers: &[String]) -> Result<ModelRef, Vec<ModelRef>> {
        let query = query.trim();
        let (name, thinking) = match query.rsplit_once(':') {
            Some((name, suffix)) if !name.is_empty() => match suffix.parse::<ThinkingLevel>() {
                Ok(level) => (name, Some(level)),
                Err(_) => (query, None),
            },
            _ => (query, None),
        };
        let reference = |model: &ModelInfo| ModelRef {
            provider: model.provider.clone(),
            model: model.id.clone(),
            thinking,
        };
        let mut candidates: Vec<&ModelInfo> = self
            .models
            .values()
            .filter(|m| providers.contains(&m.provider))
            .collect();
        candidates.sort_by_key(|m| m.key());
        let exact: Vec<&ModelInfo> = candidates
            .iter()
            .copied()
            .filter(|m| m.id.eq_ignore_ascii_case(name))
            .collect();
        match exact.as_slice() {
            [one] => return Ok(reference(one)),
            [] => {}
            many => return Err(many.iter().map(|m| reference(m)).collect()),
        }
        let pattern = Pattern::new(name, CaseMatching::Ignore, Normalization::Smart, AtomKind::Fuzzy);
        let mut matcher = Matcher::new(Config::DEFAULT);
        let mut buf = Vec::new();
        let mut scored: Vec<(u32, &ModelInfo)> = candidates
            .into_iter()
            .filter_map(|m| {
                let key = m.key();
                pattern
                    .score(Utf32Str::new(&key, &mut buf), &mut matcher)
                    .map(|score| (score, m))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.key().cmp(&b.1.key())));
        match scored.as_slice() {
            [] => Err(Vec::new()),
            [(_, only)] => Ok(reference(only)),
            [(best, model), (next, _), ..] if best > next => Ok(reference(model)),
            tied => Err(tied.iter().take(5).map(|(_, m)| reference(m)).collect()),
        }
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
    fn embedded_snapshot_is_the_catalogue_file_and_parses() {
        let json = snapshot().unwrap();
        let file = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/catalog/models.json")).unwrap();
        assert!(
            json == file,
            "the snapshot in the binary is not catalog/models.json"
        );
        serde_json::from_slice::<CatalogFile>(&json).unwrap();
        assert!(!Catalog::embedded().is_empty());
    }

    #[test]
    fn find_takes_an_exact_id_or_one_clear_fuzzy_match_and_lists_ties() {
        let mut catalog = Catalog::default();
        catalog.merge(
            [
                ("anthropic", "claude-fable-5"),
                ("anthropic", "claude-fable-5-1"),
                ("anthropic", "claude-sonnet-5"),
                ("openai", "gpt-6-sol"),
                ("other", "claude-fable-5"),
            ]
            .map(|(p, id)| ModelInfo::unknown(p, id)),
        );
        let usable = ["anthropic".to_string(), "openai".to_string()];
        let key = |r: ModelRef| r.to_string();
        assert_eq!(
            catalog.find("claude-fable-5", &usable).map(key),
            Ok("anthropic/claude-fable-5".into())
        );
        assert_eq!(
            catalog.find("sonnet", &usable).map(key),
            Ok("anthropic/claude-sonnet-5".into())
        );
        assert_eq!(
            catalog.find("SOL:high", &usable).map(key),
            Ok("openai/gpt-6-sol:high".into())
        );
        let tied = catalog.find("fable", &usable).unwrap_err();
        assert_eq!(
            tied.into_iter().map(key).collect::<Vec<_>>(),
            ["anthropic/claude-fable-5", "anthropic/claude-fable-5-1"]
        );
        assert_eq!(catalog.find("nothing-like-it", &usable), Err(Vec::new()));
        // Providers that cannot be used are not searched.
        assert_eq!(
            catalog.find("claude-fable-5", &["other".to_string()]).map(key),
            Ok("other/claude-fable-5".into())
        );
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
    fn a_listing_without_thinking_levels_keeps_the_known_ones() {
        let mut known = ModelInfo::unknown("mock", "a");
        known.thinking_levels = vec![ThinkingLevel::Low, ThinkingLevel::High];
        let mut catalog = Catalog::default();
        catalog.merge([known]);
        // As a provider's own model list describes it: a larger window, no levels.
        let mut listed = ModelInfo::unknown("mock", "a");
        listed.context_window = 400_000;
        catalog.merge([listed]);
        let model = catalog.get("mock", "a");
        assert_eq!(model.context_window, 400_000);
        assert_eq!(model.thinking_levels, [ThinkingLevel::Low, ThinkingLevel::High]);
        // An entry that names levels replaces them.
        let mut named = ModelInfo::unknown("mock", "a");
        named.thinking_levels = vec![ThinkingLevel::Max];
        catalog.merge([named]);
        assert_eq!(catalog.get("mock", "a").thinking_levels, [ThinkingLevel::Max]);
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

    #[test]
    fn a_configured_model_gets_its_name_and_thinking_levels_and_is_listed_for_its_provider() {
        use ThinkingLevel::{High, Low, Max, Medium, Minimal, Off, Xhigh};
        let overrides = BTreeMap::from([
            (
                "mock/thinker".to_string(),
                ModelOverride {
                    name: Some("Thinker".into()),
                    thinking_levels: Some(vec![Low, High]),
                    ..Default::default()
                },
            ),
            ("other/x".to_string(), ModelOverride::default()),
        ]);
        let mut catalog = Catalog::default();
        catalog.apply_overrides(&overrides);

        let thinker = catalog.get("mock", "thinker");
        assert_eq!(thinker.name.as_deref(), Some("Thinker"));
        // Naming levels says the model reasons.
        assert!(thinker.reasoning);
        // A level it does not take becomes the nearest one below, else the lowest above.
        let asked: Vec<ThinkingLevel> = [Off, Minimal, Low, Medium, High, Xhigh, Max]
            .into_iter()
            .map(|level| thinker.thinking_level(level))
            .collect();
        assert_eq!(asked, [Off, Low, Low, Low, High, High, High]);
        // A model that names no levels takes them all.
        assert_eq!(catalog.get("other", "x").thinking_level(Xhigh), Xhigh);

        let declared: Vec<String> = catalog
            .declared(&overrides, "mock")
            .into_iter()
            .map(|model| model.key())
            .collect();
        assert_eq!(declared, ["mock/thinker"]);
    }
}
