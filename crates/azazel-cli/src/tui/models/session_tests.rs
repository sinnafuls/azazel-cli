//! What `/models` asks of a session, carried out through the [`Backend`] of a session in this
//! process and of one reached through the protocol, against config files of their own.

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use azazel_protocol::{RoleSource, Roles, SaveTo};
use clap::Parser;
use tokio::io::BufReader;

use crate::app::{Active, App, discover_paths};
use crate::cli::Cli;
use crate::tui::backend::Backend;
use crate::tui::remote::{Place, Remote};

/// An application over a home and a project of its own under `dir`, and its open session.
async fn session(dir: &Path) -> (Arc<App>, Active, PathBuf) {
    let home = dir.join("home");
    let project = dir.join("project");
    std::fs::create_dir_all(project.join(".azazel")).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let cli = Cli::parse_from([
        "azazel".as_ref(),
        "--home".as_ref(),
        home.as_os_str(),
        "--cwd".as_ref(),
        project.as_os_str(),
        "--no-memory".as_ref(),
        "--set".as_ref(),
        "session.auto_name=false".as_ref(),
    ]);
    let paths = discover_paths(&cli).unwrap();
    let app = App::build(&cli, paths, true, &[]).await.unwrap();
    let active = app
        .open(
            app.requested_session(&cli).unwrap(),
            app.chain.clone(),
            app.thinking,
        )
        .await
        .unwrap();
    (Arc::new(app), active, home.join("config.toml"))
}

fn row<'a>(roles: &'a Roles, role: &str) -> &'a azazel_protocol::RoleRow {
    roles.roles.iter().find(|row| row.role == role).unwrap()
}

fn strings(specs: &[&str]) -> Option<Vec<String>> {
    Some(specs.iter().map(ToString::to_string).collect())
}

/// Assigning, thinking, fallbacks added, moved and removed, clearing, presets: what the
/// session answers and what the home config holds after each.
async fn exercise(backend: &mut dyn Backend, home_config: &Path) {
    const A: &str = "anthropic/claude-opus-4-5";
    const B: &str = "openai/gpt-5";
    let config = || std::fs::read_to_string(home_config).unwrap_or_default();
    // What `[roles]` of the home config says of smol.
    let filed = || {
        let table: toml::Table = config().parse().unwrap();
        table
            .get("roles")
            .and_then(|roles| roles.get("smol"))
            .map(ToString::to_string)
    };

    let roles = backend.roles().await.unwrap();
    assert_eq!(row(&roles, "smol").source, RoleSource::Builtin);

    let roles = backend
        .set_role("smol", strings(&[&format!("{A}:high")]), SaveTo::Home)
        .await
        .unwrap();
    assert_eq!(row(&roles, "smol").source, RoleSource::Home);
    assert_eq!(row(&roles, "smol").thinking.as_deref(), Some("high"));
    assert!(
        config().contains("smol = [\"anthropic/claude-opus-4-5:high\"]"),
        "{}",
        config()
    );

    // A fallback, then the two in the other order, then one taken away again.
    backend
        .set_role("smol", strings(&[A, B, "@default"]), SaveTo::Home)
        .await
        .unwrap();
    let roles = backend
        .set_role("smol", strings(&[A, "@default", B]), SaveTo::Home)
        .await
        .unwrap();
    assert_eq!(row(&roles, "smol").chain, [A, "@default", B]);
    let roles = backend
        .set_role("smol", strings(&[A, B]), SaveTo::Home)
        .await
        .unwrap();
    assert_eq!(row(&roles, "smol").chain, [A, B]);
    assert_eq!(
        filed().as_deref(),
        Some("[\"anthropic/claude-opus-4-5\", \"openai/gpt-5\"]")
    );

    // A preset holds the roles as they are; loading it brings them back for the session.
    let roles = backend.save_preset("mine").await.unwrap();
    assert_eq!(roles.presets, ["mine"]);
    let roles = backend.set_role("smol", None, SaveTo::Home).await.unwrap();
    assert_eq!(row(&roles, "smol").source, RoleSource::Builtin);
    assert_eq!(filed(), None, "{}", config());
    let roles = backend.apply_preset("mine").await.unwrap();
    assert_eq!(row(&roles, "smol").chain, [A, B]);
    assert_eq!(row(&roles, "smol").source, RoleSource::Session);

    // A role of your own, and a mistake.
    let roles = backend
        .set_role("critic", strings(&[B]), SaveTo::Session)
        .await
        .unwrap();
    assert_eq!(row(&roles, "critic").source, RoleSource::Session);
    assert!(
        backend
            .set_role("bad name", strings(&[B]), SaveTo::Home)
            .await
            .is_err()
    );

    // Every provider, those without a credential among them, and their models.
    let providers = backend.providers().await.unwrap();
    let anthropic = providers.iter().find(|p| p.id == "anthropic").unwrap();
    assert!(anthropic.oauth && anthropic.env.contains(&"ANTHROPIC_API_KEY".to_string()));
    assert!(anthropic.models > 0);
    let rows = backend.model_rows().await.unwrap();
    assert!(
        rows.iter()
            .any(|row| row.provider == "anthropic" && row.model.starts_with("anthropic/"))
    );
    for provider in &providers {
        let count = rows.iter().filter(|row| row.provider == provider.id).count();
        assert_eq!(count, provider.models, "{}", provider.id);
        assert!(
            rows.iter()
                .filter(|row| row.provider == provider.id)
                .all(|row| row.usable == provider.usable)
        );
    }
}

#[tokio::test]
async fn a_session_elsewhere_is_changed_through_the_protocol() {
    let dir = tempfile::tempdir().unwrap();
    let (app, active, home_config) = session(dir.path()).await;
    let (ours, theirs) = tokio::io::duplex(1 << 16);
    let (their_read, their_write) = tokio::io::split(theirs);
    let served = tokio::spawn(crate::rpc::serve(
        app,
        active,
        BufReader::new(their_read),
        their_write,
    ));
    let (our_read, our_write) = tokio::io::split(ours);
    let client = azazel_client::Client::connect(BufReader::new(our_read), our_write)
        .await
        .unwrap();
    let mut remote = Remote::open(client, Place::Ssh { host: "test".into() })
        .await
        .unwrap();
    exercise(&mut remote, &home_config).await;
    served.abort();
}
