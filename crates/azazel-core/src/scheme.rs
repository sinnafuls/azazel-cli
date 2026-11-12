//! URL schemes the `read` tool can resolve (`artifact://`, `skill://`, `agent://`, `mcp://`, …).

use std::sync::{Arc, PoisonError, RwLock};

use async_trait::async_trait;
use azazel_agent::{Access, ToolCtx};
use azazel_ai::Block;

#[async_trait]
pub trait Scheme: Send + Sync {
    /// The part before `://`.
    fn scheme(&self) -> &str;
    /// `rest` is everything after `scheme://`.
    async fn read(&self, rest: &str, ctx: &ToolCtx) -> Result<Vec<Block>, String>;
    /// What reading `rest` touches, and the subject permission rules are matched against; `None`
    /// leaves the address itself as the subject. A scheme that reaches the network answers
    /// `(Access::Net, Some("domain:<host>"))`, and is then asked about like `web_fetch`.
    fn access(&self, _rest: &str) -> (Access, Option<String>) {
        (Access::Read, None)
    }
}

/// Registered schemes; available to tools as a service.
#[derive(Default)]
pub struct Schemes {
    schemes: RwLock<Vec<(String, Arc<dyn Scheme>)>>,
}

impl Schemes {
    pub fn add(&self, owner: &str, scheme: Arc<dyn Scheme>) {
        self.schemes
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .push((owner.to_string(), scheme));
    }

    pub fn remove_owner(&self, owner: &str) {
        self.schemes
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(o, _)| o != owner);
    }

    pub fn get(&self, scheme: &str) -> Option<Arc<dyn Scheme>> {
        let schemes = self.schemes.read().unwrap_or_else(PoisonError::into_inner);
        schemes
            .iter()
            .find(|(_, s)| s.scheme() == scheme)
            .map(|(_, s)| s.clone())
    }

    /// Scheme names, sorted.
    pub fn names(&self) -> Vec<String> {
        let schemes = self.schemes.read().unwrap_or_else(PoisonError::into_inner);
        let mut names: Vec<String> = schemes.iter().map(|(_, s)| s.scheme().to_string()).collect();
        names.sort();
        names.dedup();
        names
    }
}

/// Splits `scheme://rest`. A Windows drive path (`C:\x`, `C:/x`) is not a URL.
pub fn split_url(path: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = path.split_once("://")?;
    let valid = scheme.len() >= 2
        && scheme.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    valid.then_some((scheme, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_url_accepts_schemes_and_rejects_paths() {
        assert_eq!(split_url("artifact://call_1"), Some(("artifact", "call_1")));
        assert_eq!(
            split_url("mcp://server/mem://greeting"),
            Some(("mcp", "server/mem://greeting"))
        );
        assert_eq!(
            split_url("https://example.com/a?b=c"),
            Some(("https", "example.com/a?b=c"))
        );
        for not_url in [
            "C://Users/me",
            "src/main.rs",
            "/abs/path",
            "a b://x",
            "1x://y",
            "://x",
        ] {
            assert_eq!(split_url(not_url), None, "{not_url}");
        }
    }
}
