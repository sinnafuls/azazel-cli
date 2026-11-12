//! Quick-add memory: a one-line input that starts with `# ` is remembered instead of being sent
//! to the agent.

use azazel_agent::Services;
use azazel_core::{CommandOutcome, Memory};

/// The kind a note typed by the user is stored as: something the user wants followed.
const KIND: &str = "preference";

/// What to remember, when `entered` is one line that starts with `# `.
pub(super) fn memory_note(entered: &str) -> Option<&str> {
    let line = entered.trim();
    if line.contains('\n') {
        return None;
    }
    line.strip_prefix("# ").map(str::trim).filter(|note| !note.is_empty())
}

/// Saves `note` as a memory of this project and says under which id.
pub(super) async fn remember(services: &Services, note: &str) -> CommandOutcome {
    let Some(memory) = services.get::<Memory>() else {
        return CommandOutcome::Error(
            "nothing was saved: memory is off in this session (`[memory] enabled`, --no-memory)".to_string(),
        );
    };
    match memory.0.save(note, KIND, &[], false).await {
        Ok(id) => CommandOutcome::Show(format!("remembered as {id}: {note}")),
        Err(why) => CommandOutcome::Error(format!("nothing was saved: {why}")),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use azazel_core::{MemoryApi, MemoryHit};

    use super::*;

    #[test]
    fn only_a_single_line_that_starts_with_hash_space_is_a_note() {
        assert_eq!(memory_note("# use tabs here"), Some("use tabs here"));
        assert_eq!(memory_note("  #   always run clippy  \n"), Some("always run clippy"));
        for prompt in [
            "#",
            "# ",
            "#no space",
            "## Heading",
            "# Title\nwith a body",
            "fix # this",
            "/memory",
        ] {
            assert_eq!(memory_note(prompt), None, "{prompt:?}");
        }
    }

    #[derive(Default)]
    struct Saved(Mutex<Vec<(String, String, bool)>>);

    #[async_trait]
    impl MemoryApi for Saved {
        async fn save(&self, text: &str, kind: &str, _: &[String], global: bool) -> Result<String, String> {
            if text == "fail" {
                return Err("the store is locked".to_string());
            }
            let mut saved = self.0.lock().unwrap_or_else(|e| e.into_inner());
            saved.push((text.to_string(), kind.to_string(), global));
            Ok(format!("m{}", saved.len()))
        }

        async fn search(&self, _: &str, _: usize) -> Result<Vec<MemoryHit>, String> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn a_note_is_saved_as_a_project_preference_and_its_id_is_shown() {
        let services = Services::new();
        assert!(matches!(
            remember(&services, "use tabs").await,
            CommandOutcome::Error(why) if why.starts_with("nothing was saved: memory is off")
        ));

        let store = Arc::new(Saved::default());
        services.insert(Arc::new(Memory(store.clone())));
        assert_eq!(
            remember(&services, "use tabs").await,
            CommandOutcome::Show("remembered as m1: use tabs".to_string())
        );
        assert_eq!(
            *store.0.lock().unwrap_or_else(|e| e.into_inner()),
            [("use tabs".to_string(), "preference".to_string(), false)]
        );
        assert_eq!(
            remember(&services, "fail").await,
            CommandOutcome::Error("nothing was saved: the store is locked".to_string())
        );
    }
}
