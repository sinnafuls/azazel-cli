//! The word about the lead's prompt cache running out while the session waits for its user:
//! the next request then sends the whole conversation anew, which costs more than reading it.

use std::sync::Arc;
use std::time::Instant;

use azazel_core::cache::{CacheLedger, Expiry};
use azazel_core::config::CacheWarming;
use azazel_tui::components::format_tokens;

use super::Tui;
use super::backend::Billing;

/// What is said when the cache has run out. `price` is what the cached tokens cost to send
/// anew and what reading them from the cache would have cost, in USD, when the model is paid
/// for by the token and its prices are known. `warming` is whether to name `cache.warming`: it
/// is off and could have kept this cache.
pub(super) fn notice(expiry: &Expiry, price: Option<(f64, f64)>, warming: bool) -> String {
    let mut text = format!(
        "the prompt cache of {} ran out after {} min without a request: the next one sends its {} cached tokens \
         anew",
        expiry.model,
        expiry.ttl.as_secs() / 60,
        format_tokens(expiry.prefix)
    );
    if let Some((anew, read)) = price {
        text.push_str(&format!(", about ${anew:.2} instead of ${read:.2}"));
    }
    if warming {
        text.push_str("; [cache] warming = \"idle\" keeps it alive while you think");
    }
    text
}

impl Tui {
    fn cache_ledger(&self) -> Option<Arc<CacheLedger>> {
        self.backend.local()?.app.registry.services.get::<CacheLedger>()
    }

    /// When the lead's prompt cache runs out, while the lead is idle: the moment the main loop
    /// is to say so.
    pub(super) fn cache_deadline(&self) -> Option<Instant> {
        if self.busy {
            return None;
        }
        let expiry = self.cache_ledger()?.expiry(&self.session.id())?;
        Some(expiry.at.into_std())
    }

    /// The moment the cache was to run out has come: says so, once, unless something read the
    /// cache meanwhile.
    pub(super) fn cache_tick(&mut self) {
        let Some(expiry) = self
            .cache_ledger()
            .and_then(|ledger| ledger.expired(&self.session.id()))
        else {
            return;
        };
        let Some(local) = self.backend.local() else {
            return;
        };
        let metered = self.backend.facts().billing == Billing::Metered;
        let model = local
            .app
            .router
            .catalog()
            .get(&expiry.model.provider, &expiry.model.model);
        let price = model.cost.filter(|_| metered).map(|cost| {
            let tokens = expiry.prefix as f64 / 1_000_000.0;
            // A provider without a price for writing the cache charges its input price.
            let anew = if cost.cache_write > 0.0 {
                cost.cache_write
            } else {
                cost.input
            };
            (tokens * anew, tokens * cost.cache_read)
        });
        let warming = expiry.renewable && local.app.config.cache.warming == CacheWarming::Off;
        let text = notice(&expiry, price, warming);
        self.info(&text);
    }
}

#[cfg(test)]
mod tests {}
