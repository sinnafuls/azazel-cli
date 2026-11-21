//! `azazel scripts list|show|delete`: the saved Luau scripts, without a session.

use azazel_core::Paths;
use azazel_core::scripts::{Scripts, list_text, show_text};

use crate::cli::ScriptsAction;
use crate::print::{EXIT_FAILED, EXIT_OK};

pub fn run(action: &ScriptsAction, paths: &Paths, trusted: bool) -> u8 {
    let scripts = Scripts::new(paths.clone(), trusted);
    let done = match action {
        ScriptsAction::List => Ok(list_text(&scripts.list())),
        ScriptsAction::Show { name } => scripts
            .find(name)
            .map(|script| show_text(&script))
            .ok_or_else(|| format!("there is no saved script `{name}`")),
        ScriptsAction::Delete { name } => scripts
            .delete(name)
            .map(|script| format!("deleted {}", script.path.display())),
    };
    match done {
        Ok(text) => {
            println!("{text}");
            EXIT_OK
        }
        Err(why) => {
            eprintln!("azazel: {why}");
            EXIT_FAILED
        }
    }
}
