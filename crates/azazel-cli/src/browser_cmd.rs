//! `azazel browser open [URL]`: the agents' browser profile in a window, to sign in to sites.

use azazel_core::{Config, Paths};

use crate::cli::BrowserAction;
use crate::print::{EXIT_FAILED, EXIT_OK, EXIT_USAGE};

/// Runs `azazel browser …`.
pub async fn run(action: &BrowserAction, paths: &Paths, config: &Config) -> u8 {
    match action {
        BrowserAction::Open { url } => open(url.as_deref(), paths, config).await,
    }
}

async fn open(url: Option<&str>, paths: &Paths, config: &Config) -> u8 {
    if let Some(url) = url
        && azazel_browser::page::host_of(url).is_none()
    {
        eprintln!("azazel: `{url}` is not an http or https URL");
        return EXIT_USAGE;
    }
    let Some(binary) = azazel_browser::find_binary(&config.browser.path, &|name| std::env::var(name).ok())
    else {
        eprintln!("azazel: {}", azazel_browser::NOT_FOUND);
        return EXIT_FAILED;
    };
    match azazel_browser::open_profile(&binary, &paths.browser_profile_dir(), url).await {
        Ok(()) => EXIT_OK,
        Err(why) => {
            eprintln!("azazel: {why}");
            EXIT_FAILED
        }
    }
}
