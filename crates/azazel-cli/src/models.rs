//! Model names as people type them (`--model fable`), `azazel models` and `azazel auth status`.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use azazel_ai::providers::presets::{self, ProviderConfig};
use azazel_ai::{Account, Catalog, Credential, Credentials, ModelInfo, ModelSpec, Router, VIRTUAL};
use azazel_protocol::{ModelCost, ModelRow, ProviderRow};
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
        && (known.contains(&model.provider) || azazel_ai::is_virtual(model))
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

/// What the model picker says beside a virtual model.
pub const VIRTUAL_DETAIL: &str = "picks a model for each request";

/// The virtual models extensions define, as they are chosen: `virtual/<name>`.
pub fn virtual_names(router: &Router) -> Vec<String> {
    router
        .virtual_models()
        .into_iter()
        .map(|name| format!("{VIRTUAL}/{name}"))
        .collect()
}

/// `Err` when `chain` names a virtual model no loaded extension defines, or names one while
/// `--provider only` keeps the run to one provider: a virtual model picks among the models of
/// every provider.
pub fn check_virtual(chain: &[ModelSpec], router: &Router, only: Option<&str>) -> Result<(), String> {
    let defined = router.virtual_models();
    for spec in chain {
        let ModelSpec::Model(model) = spec else {
            continue;
        };
        if !azazel_ai::is_virtual(model) {
            continue;
        }
        if let Some(only) = only {
            return Err(format!(
                "--model {}: a virtual model picks among every provider's models and cannot be used with \
                 --provider {only}",
                model.key()
            ));
        }
        if !defined.contains(&model.model) {
            let known = match defined.is_empty() {
                true => "no extension defines a virtual model".to_string(),
                false => format!("the virtual models are {}", virtual_names(router).join(", ")),
            };
            return Err(format!("--model {}: {known}", model.key()));
        }
    }
    Ok(())
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

/// Whether `provider` is used through a sign-in (a subscription), not paid for by the token:
/// the credential it would be asked with is a stored login, not a key from the flags, the
/// environment or the configuration.
pub fn login(credentials: &Credentials, provider: &ProviderConfig) -> bool {
    let Some(label) = credentials.selected(&provider.id) else {
        return false;
    };
    let keyed_elsewhere = credentials
        .source(provider)
        .is_none_or(|source| !source.starts_with("auth.json"));
    !keyed_elsewhere
        && credentials
            .accounts(&provider.id)
            .iter()
            .any(|account| account.label == label && account.login)
}

/// The cells of one row of `azazel models`: model, context, out, price, thinking, vision and
/// the roles that answer with it.
fn model_cells(m: &ModelInfo, login: bool, roles: &[String]) -> [String; 7] {
    let price = match (login, m.cost.as_ref()) {
        (true, _) => "subscription".to_string(),
        (false, Some(cost)) if cost.input == 0.0 && cost.output == 0.0 => "free".to_string(),
        (false, Some(cost)) => price(cost),
        (false, None) => "-".to_string(),
    };
    let thinking = match (m.reasoning, m.thinking_levels.is_empty()) {
        (false, _) => "-".to_string(),
        (true, true) => "yes".to_string(),
        (true, false) => m
            .thinking_levels
            .iter()
            .filter(|level| **level != azazel_ai::ThinkingLevel::Off)
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" "),
    };
    let or_dash = |text: String| if text.is_empty() { "-".to_string() } else { text };
    [
        m.key(),
        compact(m.context_window),
        compact(u64::from(m.max_output)),
        price,
        or_dash(thinking),
        if m.vision { "yes" } else { "-" }.to_string(),
        or_dash(roles.join(", ")),
    ]
}

/// The rows of `azazel models` under one heading per provider, the columns lined up across
/// all of them.
fn model_table(groups: &[(String, Vec<[String; 7]>)]) -> String {
    const HEAD: [&str; 7] = [
        "model", "context", "out", "price", "thinking", "vision", "used by",
    ];
    // Numbers are right-aligned, words left.
    const RIGHT: [bool; 7] = [false, true, true, false, false, false, false];
    let mut widths = HEAD.map(|head| head.chars().count());
    for row in groups.iter().flat_map(|(_, rows)| rows) {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let line = |cells: [&str; 7]| {
        let text: Vec<String> = cells
            .iter()
            .zip(widths)
            .zip(RIGHT)
            .map(|((cell, width), right)| match right {
                true => format!("{cell:>width$}"),
                false => format!("{cell:<width$}"),
            })
            .collect();
        format!("  {}\n", text.join("  ").trim_end())
    };
    let mut out = String::new();
    for (index, (heading, rows)) in groups.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(heading);
        out.push('\n');
        out.push_str(&line(HEAD));
        for row in rows {
            out.push_str(&line(row.each_ref().map(String::as_str)));
        }
    }
    out
}

pub(crate) fn model_json(m: &ModelInfo) -> Value {
    json!({
        "model": format!("{}/{}", m.provider, m.id),
        "name": m.name,
        "context_window": m.context_window,
        "max_output": m.max_output,
        "vision": m.vision,
        "reasoning": m.reasoning,
        "thinking_levels": m.thinking_levels,
        "cost": m.cost.map(|c| json!({ "input": c.input, "output": c.output })),
    })
}

/// A token count the way every model list shows it: `200k`, `1M`, `1.5M`.
pub fn compact(n: u64) -> String {
    match n {
        n if n >= 1_000_000 && n % 100_000 == 0 => format!("{}M", trim(n as f64 / 1e6)),
        n if n >= 1_000 => format!("{}k", n / 1_000),
        n => n.to_string(),
    }
}

/// What a million tokens in and out cost, the way every model list shows it: `$3/$15`.
pub fn price(cost: &azazel_ai::Cost) -> String {
    format!("${}/${}", trim(cost.input), trim(cost.output))
}

/// A number without the zeros a fixed precision would leave: `3`, `1.25`, `0.075`.
pub fn trim(number: f64) -> String {
    let text = format!("{number:.3}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// The roles that answer with each model first, by `provider/id`, in the order `roles` lists
/// them.
pub fn used_by(roles: &[azazel_protocol::RoleRow]) -> BTreeMap<String, Vec<String>> {
    let mut used: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in roles {
        if let Some(ModelSpec::Model(model)) = row.resolved.first().and_then(|text| text.parse().ok()) {
            used.entry(model.key()).or_default().push(row.role.clone());
        }
    }
    used
}

/// `azazel models`: the models under a heading per provider (or one JSON object per model),
/// with the roles that use each.
#[expect(clippy::too_many_arguments, reason = "the flags of the command, one by one")]
pub fn run_models(
    catalog: &Catalog,
    providers: &[ProviderConfig],
    credentials: &Credentials,
    roles: &[azazel_protocol::RoleRow],
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
    let used = used_by(roles);
    let none = Vec::new();
    let mut groups: Vec<(String, Vec<[String; 7]>)> = Vec::new();
    for p in providers {
        let mine: Vec<&&ModelInfo> = models.iter().filter(|m| m.provider == p.id).collect();
        if mine.is_empty() {
            continue;
        }
        let usable = usable(credentials, p);
        let subscription = usable && login(credentials, p);
        if json {
            for m in mine {
                let mut row = model_json(m);
                row["roles"] = json!(used.get(&m.key()).unwrap_or(&none));
                row["login"] = json!(subscription);
                row["usable"] = json!(usable);
                println!("{row}");
            }
            continue;
        }
        let credential = match (usable, subscription, credentials.source(p)) {
            (false, _, _) => "no credential".to_string(),
            (true, true, _) => "subscription".to_string(),
            (true, false, Some(source)) => source,
            (true, false, None) => "no key needed".to_string(),
        };
        let heading = format!("{} · {credential}", p.id);
        let rows = mine
            .into_iter()
            .map(|m| model_cells(m, subscription, used.get(&m.key()).unwrap_or(&none)))
            .collect();
        groups.push((heading, rows));
    }
    if !json {
        print!("{}", model_table(&groups));
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

/// The stored account of `provider` the next request would use, a sign-in or a stored key;
/// `None` when the credential comes from elsewhere (a key in the environment, which wins over
/// what is stored) or there is none.
pub fn account_in_use(credentials: &Credentials, provider: &ProviderConfig) -> Option<Account> {
    let stored = credentials
        .source(provider)
        .is_some_and(|source| source.starts_with("auth.json"));
    let label = credentials.selected(&provider.id).filter(|_| stored)?;
    credentials
        .accounts(&provider.id)
        .into_iter()
        .find(|account| account.label == label)
}

/// How long a provider may take to list its models.
pub const LIST_TIMEOUT: Duration = Duration::from_secs(20);

/// The model lists providers gave in this process, by provider: they stand in for the
/// catalogue's entries of that provider.
#[derive(Default)]
pub struct Listed(Mutex<HashMap<String, Vec<ModelInfo>>>);

impl Listed {
    fn get(&self, provider: &str) -> Option<Vec<ModelInfo>> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(provider)
            .cloned()
    }

    /// The models of `provider`: as it listed them, else as the catalogue knows them.
    fn of(&self, catalog: &Catalog, provider: &str) -> Vec<ModelInfo> {
        self.get(provider)
            .unwrap_or_else(|| catalog.for_provider(provider).into_iter().cloned().collect())
    }
}

/// Asks `provider` for the models it serves now, describes each as the catalogue does where it
/// knows the model, keeps the list in `listed` for the rest of the process and writes it to the
/// model cache (`cache`) for later ones.
pub async fn refresh(
    router: &Router,
    listed: &Listed,
    cache: &Path,
    provider: &str,
) -> Result<Vec<ModelInfo>, String> {
    let handle = router
        .provider(provider)
        .ok_or_else(|| format!("`{provider}` is not a provider"))?;
    let models = match tokio::time::timeout(LIST_TIMEOUT, handle.list_models()).await {
        Ok(Ok(models)) => models,
        Ok(Err(e)) => return Err(format!("{provider} did not list its models: {e}")),
        Err(_) => {
            return Err(format!(
                "{provider} did not list its models within {} seconds",
                LIST_TIMEOUT.as_secs()
            ));
        }
    };
    let catalog = router.catalog();
    // A list with nothing in it says nothing (discovery is off, or the endpoint lists no
    // models): the catalogue's entries stand.
    if models.is_empty() {
        return Ok(listed.of(catalog, provider));
    }
    let mut models: Vec<ModelInfo> = models
        .into_iter()
        .map(|model| match catalog.contains(&model.provider, &model.id) {
            true => catalog.get(&model.provider, &model.id),
            false => model,
        })
        .collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    models.dedup_by(|a, b| a.id == b.id);
    if let Err(e) = Catalog::refresh_cache(cache, provider, &models) {
        tracing::warn!("could not record the models of {provider}: {e}");
    }
    listed
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(provider.to_string(), models.clone());
    Ok(models)
}

/// One model as `list_models` and the `/models` screen show it.
pub fn model_row(model: &ModelInfo, usable: bool, login: bool) -> ModelRow {
    ModelRow {
        model: model.key(),
        provider: model.provider.clone(),
        id: model.id.clone(),
        name: model.name.clone(),
        context_window: model.context_window,
        max_output: u64::from(model.max_output),
        vision: model.vision,
        reasoning: model.reasoning,
        thinking_levels: model.thinking_levels.iter().map(ToString::to_string).collect(),
        cost: model.cost.map(|cost| ModelCost {
            input: cost.input,
            output: cost.output,
        }),
        usable,
        login,
    }
}

/// Whether `id` (a provider of the router) can be used, and through a sign-in. A provider the
/// configuration does not describe (the scripted one of the tests) needs no credential.
pub fn standing(id: &str, providers: &[ProviderConfig], credentials: &Credentials) -> (bool, bool) {
    match providers.iter().find(|p| p.id == id) {
        Some(config) => (usable(credentials, config), login(credentials, config)),
        None => (true, false),
    }
}

/// The models of the router's providers, provider by provider: of those that can be used, or
/// of all with `all`. `only` keeps one provider (`--provider`).
pub fn rows(
    router: &Router,
    listed: &Listed,
    providers: &[ProviderConfig],
    credentials: &Credentials,
    only: Option<&str>,
    all: bool,
) -> Vec<ModelRow> {
    let mut rows = Vec::new();
    for id in router
        .provider_ids()
        .into_iter()
        .filter(|id| only.is_none_or(|only| only == *id))
    {
        let (ready, login) = standing(id, providers, credentials);
        if !ready && !all {
            continue;
        }
        rows.extend(
            listed
                .of(router.catalog(), id)
                .iter()
                .map(|model| model_row(model, ready, login)),
        );
    }
    rows
}

/// Every provider of the router (or the one `only` keeps), as `list_providers` shows it.
pub fn provider_rows(
    router: &Router,
    listed: &Listed,
    providers: &[ProviderConfig],
    credentials: &Credentials,
    only: Option<&str>,
) -> Vec<ProviderRow> {
    router
        .provider_ids()
        .into_iter()
        .filter(|id| only.is_none_or(|only| only == *id))
        .map(|id| {
            let config = providers.iter().find(|p| p.id == id);
            let (usable, login) = standing(id, providers, credentials);
            ProviderRow {
                id: id.to_string(),
                name: config.map_or_else(|| id.to_string(), |config| config.name.clone()),
                usable,
                login,
                source: config.and_then(|config| credentials.source(config)),
                env: config
                    .map(|config| config.api_key_env.clone())
                    .unwrap_or_default(),
                oauth: config.is_some_and(|config| config.oauth.is_some()),
                models: listed.of(router.catalog(), id).len(),
            }
        })
        .collect()
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
    }

    #[test]
    fn roles_are_counted_against_the_model_they_answer_with_first() {
        let row = |role: &str, resolved: &[&str]| azazel_protocol::RoleRow {
            role: role.into(),
            chain: Vec::new(),
            source: azazel_protocol::RoleSource::Builtin,
            builtin: Vec::new(),
            resolved: resolved.iter().map(ToString::to_string).collect(),
            thinking: None,
        };
        let used = used_by(&[
            row("default", &["a/big:high", "b/other"]),
            row("smol", &["a/big"]),
            row("vision", &[]),
        ]);
        assert_eq!(used.len(), 1);
        assert_eq!(used["a/big"], ["default", "smol"]);
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
