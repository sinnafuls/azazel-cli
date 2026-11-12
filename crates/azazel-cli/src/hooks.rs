//! `/hooks`: every configured hook and how it last ran; and the `setup` hooks of `azazel init`.

use std::path::Path;
use std::sync::Arc;

use azazel_agent::Services;
use azazel_core::config::HookConfig;
use azazel_core::hooks::{HookInfo, Hooks, HooksExt, hook_sources};
use azazel_core::process::Shell;
use azazel_core::{Config, Event, ExtCtx, Headless, LoadOptions, Paths, Registry, Ui};

use crate::app::App;

fn line(hook: &HookInfo) -> String {
    let mut out = format!("  {} `{}`", hook.kind, hook.target);
    if let Some(matcher) = &hook.matcher {
        out.push_str(&format!("  matcher: {matcher}"));
    }
    if let Some(when) = &hook.when {
        out.push_str(&format!("  if: {when}"));
    }
    if !hook.options.is_empty() {
        out.push_str(&format!("  [{}]", hook.options.join(", ")));
    }
    let last = match &hook.last {
        Some(run) => format!("last run: {} in {} ms", run.result, run.duration.as_millis()),
        None => "has not run".to_string(),
    };
    out.push_str(&format!("\n    from {} · {last}", hook.source));
    out
}

/// The hooks a project's config file holds, as `(event, what it runs)`; for files that were
/// skipped because the project is not trusted.
fn hooks_in(file: &Path) -> Vec<(String, String)> {
    let Some(table) = std::fs::read_to_string(file)
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
    else {
        return Vec::new();
    };
    let Some(hooks) = table.get("hooks").and_then(toml::Value::as_table) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for (event, entries) in hooks {
        for entry in entries.as_array().into_iter().flatten() {
            let what = match entry.clone().try_into::<HookConfig>() {
                Ok(hook) => hook
                    .command
                    .or(hook.url)
                    .or(hook.prompt)
                    .unwrap_or_else(|| "(nothing to run)".to_string()),
                Err(_) => "(not a valid hook)".to_string(),
            };
            found.push((event.clone(), what));
        }
    }
    found
}

/// What `/hooks` shows: the loaded hooks by event with their source, options and last run, then
/// the hooks that were skipped and why.
pub fn report(app: &App) -> String {
    let mut out = String::new();
    let hooks = app.registry.services.get::<Hooks>();
    let listed = hooks.as_ref().map(|hooks| hooks.list()).unwrap_or_default();
    let mut event = "";
    for hook in &listed {
        if hook.event != event {
            event = hook.event;
            out.push_str(&format!("{event}\n"));
        }
        out.push_str(&line(hook));
        out.push('\n');
    }
    if hooks.as_ref().is_some_and(|hooks| !hooks.enabled()) && !listed.is_empty() {
        out.push_str("skipped: hooks are switched off for this run (--no-hooks), so none of these runs\n");
    }
    let mut skipped = Vec::new();
    if !app.trusted {
        for file in [app.paths.project_config(), app.paths.project_local_config()] {
            for (event, what) in hooks_in(&file) {
                skipped.push(format!("  {event} `{what}`\n    from {}", file.display()));
            }
        }
    }
    if !skipped.is_empty() {
        out.push_str("skipped: the project is not trusted (/trust, or start with --trust)\n");
        out.push_str(&skipped.join("\n"));
        out.push('\n');
    }
    if out.is_empty() {
        out.push_str("no hooks are configured; see docs/hooks.md\n");
    }
    out.trim_end().to_string()
}

/// Runs the `setup` hooks outside a session, for `azazel init`. Does nothing when there are none.
pub async fn run_setup(paths: &Paths, trusted: bool, trigger: &str) {
    let options = LoadOptions {
        trusted,
        overrides: toml::Table::new(),
        env: std::env::vars().collect(),
    };
    let Ok(loaded) = Config::load(paths, &options) else {
        return;
    };
    if loaded.config.hooks.get("setup").is_none_or(Vec::is_empty)
        || std::env::var_os("AZAZEL_HOOKS_DISABLED").is_some()
    {
        return;
    }
    let services = Arc::new(Services::new());
    services.insert(Arc::new(Ui(Arc::new(Headless))));
    let registry = Registry::new(ExtCtx {
        services,
        cwd: paths.cwd.clone(),
        session_id: String::new(),
    });
    let ext = HooksExt {
        sources: hook_sources(&loaded.layers),
        hooks: loaded.config.hooks,
        shell: Shell::resolve(&loaded.config.tools.shell),
        paths: paths.clone(),
        claude_compat: loaded.config.compat.claude,
        disabled: false,
        trusted,
    };
    if let Err(e) = registry.load(&ext) {
        eprintln!("azazel: setup hooks: {e}");
        return;
    }
    let mut event = Event::Setup {
        trigger: trigger.to_string(),
    };
    registry.bus.emit(&mut event).await;
}
