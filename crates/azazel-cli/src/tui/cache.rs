//! The word about the lead's prompt cache running out while the session waits for its user:
//! the next request then sends the whole conversation anew, which costs more than reading it.

use std::sync::Arc;
use std::time::Instant;

use azazel_core::cache::{CacheClock, CacheLedger, Expiry};
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

/// What is said when a large context is about to lose its cache.
pub(super) fn suggestion(clock: &CacheClock, tokens: &str) -> String {
    format!(
        "the prompt cache runs out in {} and the context holds {tokens} tokens: /handoff starts a fresh session \
         from a brief, /compact shrinks this one; either is cheapest while the cache is warm",
        clock.text()
    )
}

impl Tui {
    fn cache_ledger(&self) -> Option<Arc<CacheLedger>> {
        self.backend.local()?.app.registry.services.get::<CacheLedger>()
    }

    /// Where the lead's prompt cache stands, for the clock of the status line. Nothing while
    /// the lead works: its request reads the cache.
    pub(super) fn cache_clock(&self) -> Option<CacheClock> {
        if self.busy {
            return None;
        }
        self.cache_ledger()?.clock(&self.session.id())
    }

    /// When the clock of the status line changes next. The line is drawn again then and not
    /// before, and only while the `cache` segment is shown.
    pub(super) fn cache_clock_deadline(&self) -> Option<Instant> {
        if !self.status.shows(super::statusline::Name::Cache) {
            return None;
        }
        Some(Instant::now() + self.cache_clock()?.next_change()?)
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

    /// When the cache is a minute from running out and the context is past
    /// `[cache] suggest_handoff_at`: the moment the main loop is to suggest `/handoff`.
    pub(super) fn suggest_deadline(&self) -> Option<Instant> {
        let at = self.suggest_wanted()?;
        Some(at.into_std())
    }

    fn suggest_wanted(&self) -> Option<tokio::time::Instant> {
        let limit = self.env.config.cache.suggest_handoff_at;
        if self.busy || limit == 0 || self.backend.status().context_tokens < limit {
            return None;
        }
        self.cache_ledger()?.near_expiry_at(&self.session.id())
    }

    /// Says once per answered request that the cache is about to run out while the context is
    /// large: a handoff or a compaction now reads the cache, later it would send everything
    /// anew. Makes no request.
    pub(super) fn suggest_tick(&mut self) {
        if self.suggest_wanted().is_none() {
            return;
        }
        let Some(clock) = self
            .cache_ledger()
            .and_then(|ledger| ledger.take_near_expiry(&self.session.id()))
        else {
            return;
        };
        let tokens = format_tokens(self.backend.status().context_tokens);
        self.info(&suggestion(&clock, &tokens));
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
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn the_suggestion_names_the_time_left_and_both_ways_out() {
        let clock = CacheClock::Warm {
            left: Duration::from_secs(55),
            ttl: Duration::from_secs(300),
            prefix: 200_000,
            seq: 1,
        };
        let text = suggestion(&clock, "210k");
        assert!(text.contains("in 0:55") && text.contains("210k"));
        assert!(text.contains("/handoff") && text.contains("/compact"));
        assert!(!text.contains("  "), "{text}");
    }
}
