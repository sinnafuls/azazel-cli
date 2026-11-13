//! `azazel secrets`: the credentials agents use without seeing them, kept in `secrets.json`
//! beside `auth.json`. A request credential is added by the `http` tool to requests for its
//! host; a site login is typed into its sign-in page by the `browser` tool.

use azazel_ai::services::check_name;
use azazel_ai::{Service, ServiceStore};
use azazel_core::Paths;
use serde_json::json;

use crate::cli::SecretsAction;
use crate::login::read_secret;
use crate::print::{EXIT_FAILED, EXIT_OK, EXIT_USAGE};

/// What went wrong, and whether it was the command line.
enum Failure {
    Usage(String),
    Failed(String),
}

/// What the user wrote, or, for `-`, what is typed at the terminal without being shown.
fn typed(value: &str, prompt: &str) -> Result<String, Failure> {
    if value != "-" {
        return Ok(value.to_string());
    }
    match read_secret(prompt) {
        Ok(text) if text.is_empty() => Err(Failure::Usage("nothing was given".to_string())),
        Ok(text) => Ok(text),
        Err(e) => Err(Failure::Failed(e.to_string())),
    }
}

/// The name a site login gets when the user gives none: the host of its page.
fn site_name(pattern: &str) -> String {
    let host = pattern.split_once("://").map_or(pattern, |(_, rest)| rest);
    let host = host.split('/').next().unwrap_or_default();
    host.trim_start_matches("*.")
        .chars()
        .map(|c| match c {
            c if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') => c,
            _ => '-',
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

fn store(stored: &ServiceStore, name: &str, service: Service) -> Result<String, Failure> {
    check_name(name).map_err(Failure::Usage)?;
    let what = service.describe();
    let replaced = stored
        .store_service(name, service)
        .map_err(|e| Failure::Failed(format!("could not write {}: {e}", stored.file().display())))?;
    Ok(match replaced {
        true => format!("stored `{name}`: {what} (in place of the one stored before)"),
        false => format!("stored `{name}`: {what}"),
    })
}

/// One entry as `azazel secrets list --json` prints it: everything but the secret.
fn entry(name: &str, service: &Service) -> serde_json::Value {
    match service {
        Service::Header {
            host, name: header, ..
        } => {
            json!({"name": name, "type": "header", "host": host, "header": header})
        }
        Service::Query {
            host,
            name: parameter,
            ..
        } => json!({"name": name, "type": "query", "host": host, "parameter": parameter}),
        Service::Basic { host, username, .. } => {
            json!({"name": name, "type": "basic", "host": host, "username": username})
        }
        Service::Site { url, username, .. } => {
            json!({"name": name, "type": "site", "url": url, "username": username})
        }
    }
}

fn act(action: &SecretsAction, stored: &ServiceStore) -> Result<String, Failure> {
    match action {
        SecretsAction::Add {
            name,
            host,
            header,
            query,
            basic,
        } => {
            let service = match (header, query, basic) {
                (Some(header), None, None) => {
                    Service::header(host, &typed(header, "header (Name: value): ")?)
                }
                (None, Some(query), None) => Service::query(host, &typed(query, "parameter (name=value): ")?),
                (None, None, Some(basic)) => Service::basic(host, &typed(basic, "login (user:password): ")?),
                _ => Err("give one of --header, --query and --basic".to_string()),
            };
            store(stored, name, service.map_err(Failure::Usage)?)
        }
        SecretsAction::AddSite {
            url_pattern,
            username,
            password,
            name,
        } => {
            let password = typed(
                password.as_deref().unwrap_or("-"),
                &format!("password of {username}: "),
            )?;
            let service = Service::site(url_pattern, username, &password).map_err(Failure::Usage)?;
            let name = name.clone().unwrap_or_else(|| site_name(service.target()));
            store(stored, &name, service)
        }
        SecretsAction::List { json } => {
            let services = stored.services();
            if *json {
                let lines: Vec<String> = services
                    .iter()
                    .map(|(name, service)| entry(name, service).to_string())
                    .collect();
                return Ok(lines.join("\n"));
            }
            if services.is_empty() {
                return Ok(
                    "nothing is stored for agents; `azazel secrets add` stores a credential for a host, \
                     `azazel secrets add-site` the login of a site"
                        .to_string(),
                );
            }
            let width = services
                .keys()
                .map(|name| name.chars().count())
                .max()
                .unwrap_or(0);
            let lines: Vec<String> = services
                .iter()
                .map(|(name, service)| format!("{name:<width$}  {}", service.describe()))
                .collect();
            Ok(lines.join("\n"))
        }
        SecretsAction::Rm { name } => match stored.remove_service(name) {
            Ok(Some(service)) => Ok(format!("forgot `{name}`: {}", service.describe())),
            Ok(None) => Err(Failure::Failed(format!("nothing is stored as `{name}`"))),
            Err(e) => Err(Failure::Failed(format!(
                "could not write {}: {e}",
                stored.file().display()
            ))),
        },
    }
}

/// `azazel secrets add|add-site|list|rm`.
pub fn run(action: &SecretsAction, paths: &Paths) -> u8 {
    match act(action, &ServiceStore::new(paths.secrets_file())) {
        Ok(text) => {
            if !text.is_empty() {
                println!("{text}");
            }
            EXIT_OK
        }
        Err(Failure::Usage(why)) => {
            eprintln!("azazel: {why}");
            EXIT_USAGE
        }
        Err(Failure::Failed(why)) => {
            eprintln!("azazel: {why}");
            EXIT_FAILED
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_site_login_is_named_after_the_host_of_its_page() {
        assert_eq!(site_name("https://github.com/login*"), "github.com");
        assert_eq!(site_name("https://*.example.com"), "example.com");
        assert_eq!(site_name("http://localhost:3000/signin"), "localhost-3000");
        assert_eq!(site_name("http://[::1]:3000/"), "1--3000");
    }
}
