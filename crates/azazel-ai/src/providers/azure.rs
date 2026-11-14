//! Azure OpenAI with Microsoft Entra: tokens from the Azure CLI (`[providers.azure] auth =
//! "azure-cli"`), and the deployments of a resource for the model list.
//!
//! The deployment inventory Azure documents is Azure Resource Manager, which takes Entra tokens
//! only; a resource key reaches the resource's own `/openai/deployments`, which only the data-plane
//! API version 2022-12-01 still has. With an Azure CLI sign-in the first is asked, the second when
//! the first fails (no read access to the subscription).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use reqwest::header::HeaderMap;
use serde_json::{Value, json};

use crate::catalog::ModelInfo;
use crate::credentials::{Auth, now_ms};
use crate::error::AiError;
use crate::providers::http::{HttpProvider, Wire};
use crate::providers::presets::AuthStyle;
use crate::wire;

/// The resource an Azure OpenAI request token is issued for.
pub const COGNITIVE_SERVICES: &str = "https://cognitiveservices.azure.com";
/// Azure Resource Manager, which lists a resource's deployments to an Entra token.
pub const MANAGEMENT: &str = "https://management.azure.com";
const DATA_PLANE_VERSION: &str = "2022-12-01";
const ARM_VERSION: &str = "2024-10-01";
const GRAPH_VERSION: &str = "2022-10-01";
/// A token with less than this left is asked for again.
const MARGIN_MS: u64 = 5 * 60 * 1000;
/// How long a token is used when the CLI does not say (versions before 2.54).
const UNKNOWN_LIFETIME_MS: u64 = 10 * 60 * 1000;
const CLI_TIMEOUT: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// The most pages of deployments that are read.
const MAX_PAGES: usize = 25;
/// The host names an Azure OpenAI or AI Services resource answers at; the first label is its name.
const RESOURCE_HOSTS: [&str; 3] = [".openai.azure.com", ".cognitiveservices.azure.com", ".services.ai.azure.com"];

/// Entra tokens from the Azure CLI, one per resource, used until shortly before they run out.
#[derive(Debug, Default)]
pub struct AzureCli {
    /// The program to run instead of `az` from `PATH`.
    program: Option<PathBuf>,
    tokens: Mutex<HashMap<String, (String, u64)>>,
}

impl AzureCli {
    /// An Azure CLI found at `program`.
    pub fn with_program(program: PathBuf) -> Self {
        AzureCli {
            program: Some(program),
            tokens: Mutex::default(),
        }
    }

    /// A token for `resource` (`https://cognitiveservices.azure.com`), from the CLI when the one
    /// held runs out within five minutes.
    pub async fn token(&self, resource: &str) -> Result<String, AiError> {
        let held = self.cache().get(resource).cloned();
        if let Some((token, expires)) = held
            && expires > now_ms() + MARGIN_MS
        {
            return Ok(token);
        }
        let (token, expires) = self.ask(resource).await?;
        self.cache()
            .insert(resource.to_string(), (token.clone(), expires));
        Ok(token)
    }

    /// Forgets the tokens held, after a server refused one.
    pub fn forget(&self) {
        self.cache().clear();
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, HashMap<String, (String, u64)>> {
        self.tokens.lock().unwrap_or_else(PoisonError::into_inner)
    }

    async fn ask(&self, resource: &str) -> Result<(String, u64), AiError> {
        let program = match &self.program {
            Some(program) => program.clone(),
            None => which::which("az").map_err(|_| {
                AiError::Auth(
                    "`auth = \"azure-cli\"` needs the Azure CLI: `az` is not on PATH. Install it and \
                     run `az login`"
                        .to_string(),
                )
            })?,
        };
        let run = tokio::process::Command::new(program)
            .args(["account", "get-access-token", "--resource", resource, "--output", "json"])
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        let fail = |why: String| AiError::Auth(format!("az account get-access-token failed: {why}"));
        let output = tokio::time::timeout(CLI_TIMEOUT, run)
            .await
            .map_err(|_| fail(format!("no answer in {} seconds", CLI_TIMEOUT.as_secs())))?
            .map_err(|e| fail(e.to_string()))?;
        if !output.status.success() {
            let said = String::from_utf8_lossy(&output.stderr);
            return Err(fail(format!("{}; run `az login`", said.trim())));
        }
        token_answer(&output.stdout)
    }
}

/// The token and when it runs out (Unix milliseconds) in what `az account get-access-token`
/// prints.
fn token_answer(stdout: &[u8]) -> Result<(String, u64), AiError> {
    let unreadable = || AiError::Auth("az account get-access-token printed no token".to_string());
    let answer: Value = serde_json::from_slice(stdout).map_err(|_| unreadable())?;
    let token = answer
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(unreadable)?;
    let expires = answer
        .get("expires_on")
        .and_then(|value| value.as_u64().or_else(|| value.as_str()?.trim().parse().ok()))
        .map_or_else(|| now_ms() + UNKNOWN_LIFETIME_MS, |seconds| seconds.saturating_mul(1000));
    Ok((token.to_string(), expires))
}

/// The name of the resource at `base_url`, when it is an Azure resource address; only letters,
/// digits and hyphens, as Azure allows, since it goes into a query.
pub fn resource_name(base_url: &str) -> Option<&str> {
    let host = base_url.split("://").nth(1)?.split(['/', ':']).next()?;
    let name = RESOURCE_HOSTS
        .iter()
        .find_map(|suffix| host.strip_suffix(suffix))?;
    let valid = !name.is_empty()
        && !name.starts_with('-')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    valid.then_some(name)
}

/// A deployment: its name, and the model it serves.
type Deployment = (String, String);

/// The deployments of the provider's resource as models: `id` is the deployment, the name says
/// the model when it is called otherwise.
pub(super) async fn deployments(
    provider: &HttpProvider,
    wire: &Wire,
    auth: &Auth,
) -> Result<Vec<ModelInfo>, AiError> {
    let config = provider.config_for(auth);
    let mut found = None;
    if config.auth_style == AuthStyle::AzureCli
        && let Some(name) = resource_name(&config.base_url)
    {
        let cli = provider.credentials().azure_cli();
        match manager_deployments(provider.client(), cli, MANAGEMENT, name).await {
            Ok(listed) => found = Some(listed),
            Err(why) => tracing::debug!("Azure Resource Manager did not list {name}: {why}"),
        }
    }
    let mut found = match found {
        Some(found) => found,
        None => resource_deployments(provider, wire, &config, auth).await?,
    };
    found.sort();
    Ok(found
        .into_iter()
        .map(|(deployment, model)| ModelInfo {
            name: (model != deployment).then_some(model),
            ..ModelInfo::unknown(&config.id, &deployment)
        })
        .collect())
}

/// `GET {resource}/openai/deployments`, with the key or the token.
async fn resource_deployments(
    provider: &HttpProvider,
    wire: &Wire,
    config: &crate::providers::presets::ProviderConfig,
    auth: &Auth,
) -> Result<Vec<Deployment>, AiError> {
    let mut headers = HeaderMap::new();
    wire::add_headers(&mut headers, config, auth, wire::KeyHeader::Bearer);
    let url = format!(
        "{}/openai/deployments?api-version={DATA_PLANE_VERSION}",
        wire::azure::resource(&config.base_url)
    );
    let request = provider
        .client()
        .get(url)
        .headers(headers)
        .timeout(REQUEST_TIMEOUT);
    let body = provider.fetch_json(wire, request).await?;
    Ok(body
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item.get("status").and_then(Value::as_str) == Some("succeeded"))
        .filter_map(|item| {
            let text = |name: &str| item.get(name).and_then(Value::as_str).filter(|t| !t.is_empty());
            Some((text("id")?.to_string(), text("model")?.to_string()))
        })
        .collect())
}

async fn manager_json(request: reqwest::RequestBuilder, token: &str) -> Result<Value, AiError> {
    let response = request
        .bearer_auth(token)
        .header("accept", "application/json")
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| AiError::Network(e.to_string()))?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        let shown: String = text.chars().take(200).collect();
        return Err(AiError::Auth(format!("HTTP {}: {shown}", status.as_u16())));
    }
    response.json().await.map_err(|e| AiError::Network(e.to_string()))
}

/// The deployments Azure Resource Manager at `management` lists for the resource called `name`,
/// found through Resource Graph in every subscription the CLI's account can read.
pub async fn manager_deployments(
    http: &reqwest::Client,
    cli: &AzureCli,
    management: &str,
    name: &str,
) -> Result<Vec<Deployment>, AiError> {
    let token = cli.token(MANAGEMENT).await?;
    let query = format!(
        "resources | where type =~ 'microsoft.cognitiveservices/accounts' and kind in~ ('AIServices', \
         'OpenAI') | extend resourceName = tostring(properties.customSubDomainName) | where \
         resourceName =~ '{name}' | project id | take 1"
    );
    let graph = http
        .post(format!(
            "{management}/providers/Microsoft.ResourceGraph/resources?api-version={GRAPH_VERSION}"
        ))
        .json(&json!({ "query": query }));
    let answer = manager_json(graph, &token).await?;
    let id = answer
        .pointer("/data/0/id")
        .and_then(Value::as_str)
        .filter(|id| id.starts_with('/'))
        .ok_or_else(|| AiError::Auth(format!("Azure Resource Graph knows no resource {name}")))?;
    let mut url = format!("{management}{id}/deployments?api-version={ARM_VERSION}");
    let mut found = Vec::new();
    for _ in 0..MAX_PAGES {
        let page = manager_json(http.get(&url), &token).await?;
        for item in page.get("value").and_then(Value::as_array).into_iter().flatten() {
            let text = |at: &str| item.pointer(at).and_then(Value::as_str).filter(|t| !t.is_empty());
            if text("/properties/provisioningState") != Some("Succeeded") {
                continue;
            }
            if let (Some(deployment), Some(model)) = (text("/name"), text("/properties/model/name")) {
                found.push((deployment.to_string(), model.to_string()));
            }
        }
        match page.get("nextLink").and_then(Value::as_str) {
            // The token goes with every page, so a page must stay on the manager's own address.
            Some(next) if next.starts_with(&format!("{management}/")) => url = next.to_string(),
            Some(_) => {
                return Err(AiError::Auth(
                    "Azure Resource Manager pointed to a page elsewhere".to_string(),
                ));
            }
            None => break,
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cli_answer_gives_the_token_and_when_it_runs_out() {
        let (token, expires) =
            token_answer(br#"{"accessToken":"t-1","expiresOn":"2030-01-01 00:00:00.000000","expires_on":1893456000}"#)
                .unwrap();
        assert_eq!((token.as_str(), expires), ("t-1", 1_893_456_000_000));
        // Older versions print only a local time; the token is then used for a short while.
        let (_, assumed) = token_answer(br#"{"accessToken":"t-2","expiresOn":"2030-01-01"}"#).unwrap();
        assert!(assumed.saturating_sub(now_ms()) <= UNKNOWN_LIFETIME_MS);
        assert!(token_answer(b"not json").is_err());
        assert!(token_answer(br#"{"accessToken":""}"#).is_err());
    }

    #[test]
    fn only_azure_resource_addresses_name_a_resource() {
        assert_eq!(resource_name("https://my-res.openai.azure.com"), Some("my-res"));
        assert_eq!(
            resource_name("https://res2.cognitiveservices.azure.com/openai/v1"),
            Some("res2")
        );
        assert_eq!(resource_name("https://hub.services.ai.azure.com/models"), Some("hub"));
        assert_eq!(resource_name("https://gw.example/azure"), None);
        assert_eq!(resource_name("http://127.0.0.1:9/openai"), None);
        assert_eq!(resource_name("https://a'b.openai.azure.com"), None);
    }
}
