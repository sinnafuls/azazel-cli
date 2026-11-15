//! Wiring for coordinator mode, the advisor and typed results: what the command line and the
//! configuration ask for, handed to the team crate, which does the work.

use std::path::Path;
use std::sync::Arc;

use azazel_core::config::AdvisorWhen;
use azazel_core::{Config, EntryKind, ExtError, Registry, SessionStore};
use azazel_team::coordinator::{entry, recorded};
use azazel_team::{Advisor, AdvisorExt, AdvisorSettings, Coordinator, CoordinatorExt, Typed};

use crate::app::UsageError;
use crate::cli::Cli;

/// Registers coordinator mode and the advisor. Call before the goal extension is loaded: the
/// advisor's review of a stop then comes before a goal's own check, which keeps the last word.
pub fn load(registry: &Registry, config: &Config, cli: &Cli) -> Result<Arc<Coordinator>, ExtError> {
    let coordinator = Arc::new(Coordinator::new(config.team.coordinator || cli.coordinator));
    registry.load(&CoordinatorExt {
        coordinator: coordinator.clone(),
    })?;
    registry.load(&AdvisorExt {
        advisor: Arc::new(Advisor::new(AdvisorSettings {
            chain: config.advisor.model.clone(),
            on_stop: config.advisor.when == AdvisorWhen::Stop,
        })),
    })?;
    Ok(coordinator)
}

/// The schema `--json-schema` names: JSON given inline, or the file that holds it.
pub fn result_schema(value: &str, cwd: &Path) -> Result<Arc<Typed>, UsageError> {
    let text = if value.trim_start().starts_with('{') {
        value.to_string()
    } else {
        std::fs::read_to_string(cwd.join(value))
            .map_err(|e| UsageError(format!("--json-schema {value}: {e}")))?
    };
    let schema = serde_json::from_str(&text)
        .map_err(|e| UsageError(format!("--json-schema: the schema is not valid JSON: {e}")))?;
    Typed::new(schema)
        .map(Arc::new)
        .map_err(|why| UsageError(format!("--json-schema: {why}")))
}

/// Puts coordinator mode to what `session` was left in, or leaves it as it is when the session
/// never recorded one; `forced` (`--coordinator`) turns it on whatever the session says. A
/// session that is opened in another mode than it records gets the new one written down, so that
/// resuming it finds the mode again.
pub fn open_session(coordinator: &Coordinator, session: &SessionStore, forced: bool) {
    let before = recorded(&session.branch(None));
    let on = forced || before.unwrap_or_else(|| coordinator.is_on());
    coordinator.restore(on);
    if before.unwrap_or(false) != on {
        session.append(EntryKind::ModeChange { mode: entry(on) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_keeps_its_coordinator_mode_and_a_new_mode_is_written_down() {
        let dir = tempfile::tempdir().unwrap();
        let modes = |session: &SessionStore| -> Vec<String> {
            session
                .branch(None)
                .into_iter()
                .filter_map(|entry| match entry.kind {
                    EntryKind::ModeChange { mode } => Some(mode),
                    _ => None,
                })
                .collect()
        };
        // A new session in a run where the mode is off records nothing.
        let coordinator = Coordinator::new(false);
        let plain = SessionStore::create(dir.path(), dir.path(), None).unwrap();
        open_session(&coordinator, &plain, false);
        assert!(!coordinator.is_on() && modes(&plain).is_empty());

        // `--coordinator` turns it on and the session remembers.
        open_session(&coordinator, &plain, true);
        assert!(coordinator.is_on());
        assert_eq!(modes(&plain), ["coordinator:on"]);

        // Another run, where the mode is off by default, finds it on in that session...
        let later = Coordinator::new(false);
        let reopened = SessionStore::open(&plain.path()).unwrap();
        open_session(&later, &reopened, false);
        assert!(later.is_on());
        assert_eq!(modes(&reopened), ["coordinator:on"]);

        // ...and a session that was switched off stays off where the default is on.
        reopened.append(EntryKind::ModeChange { mode: entry(false) });
        let default_on = Coordinator::new(true);
        open_session(&default_on, &reopened, false);
        assert!(!default_on.is_on());
        assert_eq!(modes(&reopened), ["coordinator:on", "coordinator:off"]);
    }
}
