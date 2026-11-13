//! Model names as people type them (`--model fable`), `azazel models` and `azazel auth status`.

use std::collections::BTreeMap;

use azazel_ai::providers::presets::{self, ProviderConfig};
use azazel_ai::{Catalog, Credential, Credentials, ModelInfo, ModelSpec};
use serde_json::{Value, json};

use crate::print::{EXIT_FAILED, EXIT_OK};

/// Turns `--model` texts into specs, in this order: `@role`; `provider/id` with a known provider,
/// as given; the model of `usable` providers whose id is the text; the single best fuzzy match
/// among them. `Err` names the text and, when several models match, the best candidates.
pub fn resolve_specs(
    texts: &[String],
    catalog: &Catalog,
    known: &[String],
    usable: &[String],
) -> Result<Vec<ModelSpec>, String> {
    texts
        .iter()
        .map(|text| resolve_spec(text.trim(), catalog, known, usable))
        .collect()
}

fn resolve_spec(
    text: &str,
    catalog: &Catalog,
    known: &[String],
    usable: &[String],
) -> Result<ModelSpec, String> {
    if text.starts_with('@') {
        return text
            .parse::<ModelSpec>()
            .map_err(|e| format!("--model {text}: {e}"));
    }
    if let Ok(spec @ ModelSpec::Model(_)) = text.parse::<ModelSpec>()
        && let ModelSpec::Model(model) = &spec
        && known.contains(&model.provider)
    {
        return Ok(spec);
    }
    match catalog.find(text, usable) {
        Ok(model) => Ok(ModelSpec::Model(model)),
        Err(candidates) if candidates.is_empty() => Err(format!(
            "--model {text}: no model of a provider you can use matches; `azazel models` lists them"
        )),
        Err(candidates) => Err(format!(
            "--model {text} matches several models: {}; give more of the name",
            candidates
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Whether `provider` can be asked: it needs no key, or one is available.
pub fn usable(credentials: &Credentials, provider: &ProviderConfig) -> bool {
    !provider.requires_key || credentials.source(provider).is_some()
}

/// The models `azazel models` lists: of providers with a credential, or of all with `all`.
pub fn listed<'a>(
    catalog: &'a Catalog,
    providers: &[ProviderConfig],
    credentials: &Credentials,
    query: Option<&str>,
    provider: Option<&str>,
    all: bool,
) -> Vec<&'a ModelInfo> {
    let wanted: Vec<String> = providers
        .iter()
        .filter(|p| all || usable(credentials, p))
        .filter(|p| provider.is_none_or(|id| p.id == id))
        .map(|p| p.id.clone())
        .collect();
    let mut models: Vec<&ModelInfo> = wanted.iter().flat_map(|id| catalog.for_provider(id)).collect();
    if let Some(query) = query.map(str::trim).filter(|q| !q.is_empty()) {
        let query = query.to_ascii_lowercase();
        models.retain(|m| {
            let key = format!("{}/{}", m.provider, m.id).to_ascii_lowercase();
            // Every character of the query, in order: the same loose idea as the fuzzy matcher.
            let mut rest = key.chars();
            query.chars().all(|c| rest.any(|k| k == c))
        });
    }
    models
}

fn model_line(m: &ModelInfo) -> String {
    let price = m
        .cost
        .map(|c| format!("${}/${}", trim(c.input), trim(c.output)))
        .unwrap_or_else(|| "-".to_string());
    format!(
        "{:<44} {:>9} {:>9}  {:<6} {:<8} {price}",
        format!("{}/{}", m.provider, m.id),
        compact(m.context_window),
        compact(u64::from(m.max_output)),
        if m.vision { "vision" } else { "" },
        if m.reasoning { "thinking" } else { "" },
    )
}

pub(crate) fn model_json(m: &ModelInfo) -> Value {
    json!({
        "model": format!("{}/{}", m.provider, m.id),
        "context_window": m.context_window,
        "max_output": m.max_output,
        "vision": m.vision,
        "reasoning": m.reasoning,
        "cost": m.cost.map(|c| json!({ "input": c.input, "output": c.output })),
    })
}

fn compact(n: u64) -> String {
    match n {
        n if n >= 1_000_000 && n % 1_000_000 == 0 => format!("{}M", n / 1_000_000),
        n if n >= 1_000 => format!("{}k", n / 1_000),
        n => n.to_string(),
    }
}

fn trim(price: f64) -> String {
    let text = format!("{price:.3}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// `azazel models`: one line (or JSON object) per model.
pub fn run_models(
    catalog: &Catalog,
    providers: &[ProviderConfig],
    credentials: &Credentials,
    query: Option<&str>,
    provider: Option<&str>,
    all: bool,
    json: bool,
) -> u8 {
    if let Some(id) = provider
        && !providers.iter().any(|p| p.id == id)
    {
        eprintln!("azazel: no provider `{id}`");
        return EXIT_FAILED;
    }
    let models = listed(catalog, providers, credentials, query, provider, all);
    for m in &models {
        if json {
            println!("{}", model_json(m));
        } else {
            println!("{}", model_line(m));
        }
    }
    if models.is_empty() && !json {
        eprintln!(
            "{}",
            if all {
                "no model matches"
            } else {
                "no model matches among the providers you have a credential for (--all lists every one)"
            }
        );
    }
    EXIT_OK
}

/// `azazel auth status`: where each provider's credential comes from; secrets are never printed.
pub fn run_auth_status(providers: &[ProviderConfig], credentials: &Credentials, json: bool) -> u8 {
    let stored = credentials.load();
    let rows = status_rows(providers, credentials, &stored, azazel_ai::credentials::now_ms());
    if rows.is_empty() && !json {
        println!("no provider has a credential: set an API key variable or run `azazel login`");
    }
    for row in rows {
        if json {
            println!("{row}");
        } else {
            let mut line = format!(
                "{:<20} {}",
                row["provider"].as_str().unwrap_or(""),
                row["source"].as_str().unwrap_or("")
            );
            if let Some(account) = row["account"].as_str() {
                line.push_str(&format!("  {account}"));
            }
            if let Some(expires) = row["expires_in"].as_str() {
                line.push_str(&format!("  {expires}"));
            }
            if let Some(accounts) = row["accounts"].as_array() {
                let labels: Vec<&str> = accounts.iter().filter_map(Value::as_str).collect();
                line.push_str(&format!("  accounts: {}", labels.join(", ")));
            }
            println!("{line}");
        }
    }
    EXIT_OK
}

fn status_rows(
    providers: &[ProviderConfig],
    credentials: &Credentials,
    stored: &BTreeMap<String, Credential>,
    now_ms: u64,
) -> Vec<Value> {
    providers
        .iter()
        .filter_map(|provider| {
            let source = credentials.source(provider)?;
            let mut row = json!({ "provider": provider.id, "source": source });
            // The stored accounts beside the one in use, when there are several.
            let accounts = credentials.accounts(&provider.id);
            if accounts.len() > 1 {
                row["accounts"] = accounts.iter().map(|account| account.label.as_str()).collect();
            }
            let in_use = credentials.selected(&provider.id).unwrap_or_default();
            if source.contains("login")
                && let Some(Credential::OAuth { expires, extra, .. }) =
                    stored.get(&azazel_ai::credentials::account_key(&provider.id, &in_use))
            {
                if let Some(account) = ["email", "account_email", "org_name"]
                    .iter()
                    .find_map(|key| extra.get(*key).and_then(Value::as_str))
                {
                    row["account"] = json!(account);
                }
                row["expires_in"] = json!(match expires.checked_sub(now_ms) {
                    _ if *expires >= azazel_ai::oauth::map::NEVER => "does not expire".to_string(),
                    Some(left) => format!("expires in {}", human(left / 1000)),
                    None => "expired; refreshed on next use".to_string(),
                });
            }
            Some(row)
        })
        .collect()
}

fn human(secs: u64) -> String {
    match secs {
        s if s >= 86_400 => format!("{}d", s / 86_400),
        s if s >= 3_600 => format!("{}h", s / 3_600),
        s => format!("{}m", s / 60),
    }
}

/// Every provider the configuration knows, in id order.
pub fn providers(config: &azazel_core::Config) -> Result<Vec<ProviderConfig>, String> {
    presets::resolve(&config.providers)
}

#[cfg(test)]
mod tests {
    use azazel_ai::{ModelRef, ProviderSettings};

    use super::*;

    fn catalog(ids: &[&str]) -> Catalog {
        let mut catalog = Catalog::default();
        catalog.merge(ids.iter().map(|key| {
            let (provider, id) = key.split_once('/').unwrap();
            ModelInfo::unknown(provider, id)
        }));
        catalog
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn names_resolve_by_role_exact_id_then_fuzzy_among_usable_providers() {
        let catalog = catalog(&[
            "anthropic/claude-fable-5",
            "anthropic/claude-haiku-4-5",
            "openai/gpt-6-sol",
            "openai/gpt-6-luna",
            "local/claude-fable-5",
        ]);
        let known = strings(&["anthropic", "openai", "local", "ollama"]);
        let usable = strings(&["anthropic", "openai"]);
        let one = |text: &str| resolve_specs(&[text.to_string()], &catalog, &known, &usable);
        let model = |p: &str, m: &str| vec![ModelSpec::Model(ModelRef::new(p, m))];

        assert_eq!(one("@smol").unwrap(), vec![ModelSpec::Role("smol".into())]);
        // A known provider is taken at its word, listed or not (local servers name their own).
        assert_eq!(one("ollama/qwen3:8b").unwrap(), model("ollama", "qwen3:8b"));
        // The same id at an unusable provider does not make it ambiguous.
        assert_eq!(
            one("claude-fable-5").unwrap(),
            model("anthropic", "claude-fable-5")
        );
        assert_eq!(one("fable").unwrap(), model("anthropic", "claude-fable-5"));
        let thinking = one("haiku:high").unwrap();
        assert_eq!(thinking[0].to_string(), "anthropic/claude-haiku-4-5:high");

        let tie = one("gpt-6").unwrap_err();
        assert!(
            tie.contains("openai/gpt-6-luna") && tie.contains("openai/gpt-6-sol"),
            "{tie}"
        );
        assert!(one("zzzz").unwrap_err().contains("azazel models"));
    }

    #[test]
    fn listing_filters_by_credential_provider_and_query() {
        let catalog = catalog(&[
            "anthropic/claude-fable-5",
            "openai/gpt-6-sol",
            "openai/gpt-6-luna",
        ]);
        let mut settings = BTreeMap::new();
        settings.insert(
            "openai".to_string(),
            ProviderSettings {
                api_key: Some("k".into()),
                ..ProviderSettings::default()
            },
        );
        let providers = presets::resolve(&settings).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let credentials =
            Credentials::with_env(dir.path().join("auth.json"), std::sync::Arc::new(|_: &str| None));
        let ids = |models: Vec<&ModelInfo>| models.iter().map(|m| m.id.clone()).collect::<Vec<_>>();

        assert_eq!(
            ids(listed(&catalog, &providers, &credentials, None, None, false)),
            ["gpt-6-luna", "gpt-6-sol"]
        );
        assert_eq!(
            ids(listed(
                &catalog,
                &providers,
                &credentials,
                Some("sol"),
                None,
                false
            )),
            ["gpt-6-sol"]
        );
        assert_eq!(
            ids(listed(
                &catalog,
                &providers,
                &credentials,
                None,
                Some("anthropic"),
                true
            )),
            ["claude-fable-5"]
        );
        let line = model_line(&catalog.get("openai", "gpt-6-sol"));
        assert!(
            line.starts_with("openai/gpt-6-sol") && line.contains("128k"),
            "{line}"
        );
    }

    #[test]
    fn status_names_sources_and_login_accounts_without_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let credentials = Credentials::with_env(
            dir.path().join("auth.json"),
            std::sync::Arc::new(|name: &str| (name == "OPENAI_API_KEY").then(|| "sk-secret".to_string())),
        );
        let mut extra = BTreeMap::new();
        extra.insert("email".to_string(), json!("me@example.com"));
        credentials
            .store(
                "anthropic",
                Credential::OAuth {
                    access: "at-secret".into(),
                    refresh: "rt-secret".into(),
                    expires: 2 * 3_600_000 + 5_000,
                    extra,
                },
            )
            .unwrap();
        let providers = presets::resolve(&BTreeMap::new()).unwrap();
        let rows = status_rows(&providers, &credentials, &credentials.load(), 5_000);
        let text = serde_json::to_string(&rows).unwrap();
        assert!(!text.contains("secret"), "{text}");
        let anthropic = rows.iter().find(|r| r["provider"] == "anthropic").unwrap();
        assert_eq!(anthropic["account"], "me@example.com");
        assert_eq!(anthropic["expires_in"], "expires in 2h");
        let openai = rows.iter().find(|r| r["provider"] == "openai").unwrap();
        assert_eq!(openai["source"], "env OPENAI_API_KEY");
    }
}
