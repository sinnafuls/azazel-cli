//! `/models`: one screen for choosing the model of every role, its fallbacks and its thinking
//! level, the ctrl+p cycle and presets, with the models of every provider beside them. Changes
//! go through the [`Backend`](super::backend::Backend), so a session elsewhere is changed the
//! same way; the screen itself ([`Models`]) only says what is to be done.

mod list;
mod roles_view;
mod screen;
#[cfg(test)]
mod session_tests;
mod sidebar;
mod strip;
#[cfg(test)]
mod tests;

use std::time::Instant;

use azazel_core::Ui;
use azazel_protocol::{ModelRow, Roles, SaveTo};
use azazel_tui::Role;

pub(crate) use self::screen::{Data, Models, Opening, Step};
use super::bridge::Reply;
use super::input::FavouriteChange;
use super::overlay::{Action, Kind, Overlay};
use super::{Internal, Tui};

/// What the screen asks the UI to do.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Request {
    /// See [`Backend::set_role`](super::backend::Backend::set_role).
    SetRole {
        role: String,
        chain: Option<Vec<String>>,
        save: SaveTo,
    },
    SavePreset(String),
    ApplyPreset(String),
    /// Ask this provider for its models.
    Refresh(String),
    /// Change what ctrl+p steps through.
    Cycle {
        spec: String,
        change: FavouriteChange,
    },
    /// Sign in to this provider, or store a key for it.
    SignIn(String),
    AddKey(String),
}

/// What reaches the open screen from work done beside the UI loop.
pub(crate) enum Update {
    /// A provider listed its models, or could not.
    Refreshed {
        provider: String,
        result: Result<Vec<ModelRow>, String>,
    },
    /// A sign-in or a key ended: the line that says what it came to, `None` when it was given
    /// up.
    SignedIn {
        provider: String,
        result: Result<Option<String>, String>,
    },
}

impl Tui {
    /// The open screen, or the one waiting for a sign-in to end.
    fn models_screen(&mut self) -> Option<&mut Models> {
        let open = matches!(
            &self.overlay,
            Some(Overlay {
                kind: Kind::Models(_),
                ..
            })
        );
        let overlay = match open {
            true => self.overlay.as_mut(),
            false => self.models_parked.as_mut(),
        }?;
        match &mut overlay.kind {
            Kind::Models(screen) => Some(screen),
            _ => None,
        }
    }

    /// What the screen shows, as the session has it now. What could not be read is left
    /// empty and said.
    async fn models_data(&mut self) -> (Data, Vec<String>) {
        let mut problems = Vec::new();
        let rows = self.backend.model_rows().await.unwrap_or_else(|e| {
            problems.push(format!("the models could not be listed: {e}"));
            Vec::new()
        });
        let providers = self.backend.providers().await.unwrap_or_else(|e| {
            problems.push(format!("the providers could not be listed: {e}"));
            Vec::new()
        });
        let roles = self.backend.roles().await.unwrap_or_else(|e| {
            problems.push(format!("the roles could not be listed: {e}"));
            Roles {
                roles: Vec::new(),
                presets: Vec::new(),
                project: false,
            }
        });
        let data = Data {
            rows,
            providers,
            roles,
            current: self.backend.current_model(),
            recent: super::recent::load(&super::recent::file(&self.env.paths)),
            cycle: self.favourites.clone(),
        };
        (data, problems)
    }

    /// `/models`: opens on the catalogue at once and asks the providers that can be used for
    /// their models beside it, each once per process. `done` hears when the screen closes.
    pub(super) async fn open_models(&mut self, opening: Opening, done: Option<Reply<()>>) {
        let (data, problems) = self.models_data().await;
        let mut screen = Models::new(data, &opening, &self.models_asked, self.theme.glyphs());
        if let Some(problem) = problems.first() {
            screen.say(Role::Error, problem);
        }
        for provider in screen.start_refreshes() {
            self.models_asked.insert(provider.clone());
            self.backend.refresh_models(provider, self.internal_tx.clone());
        }
        self.overlay = Some(Overlay::models(Box::new(screen), Action::Models { done }));
    }

    /// Carries out what the screen asked for and tells it how that went.
    pub(super) async fn models_request(&mut self, request: Request) {
        match request {
            Request::SetRole { role, chain, save } => {
                let result = self.backend.set_role(&role, chain, save).await;
                self.roles_changed(result);
            }
            Request::SavePreset(name) => {
                let result = self.backend.save_preset(&name).await;
                self.roles_changed(result);
            }
            Request::ApplyPreset(name) => {
                let result = self.backend.apply_preset(&name).await;
                self.roles_changed(result);
            }
            Request::Refresh(provider) => {
                self.models_asked.insert(provider.clone());
                self.backend.refresh_models(provider, self.internal_tx.clone());
            }
            Request::Cycle { spec, change } => self.change_favourite(&spec, change),
            Request::SignIn(provider) => self.models_sign_in(provider, false),
            Request::AddKey(provider) => self.models_sign_in(provider, true),
        }
    }

    fn roles_changed(&mut self, result: Result<Roles, String>) {
        let current = self.backend.current_model();
        if let Some(screen) = self.models_screen() {
            screen.roles_changed(result, current);
        }
    }

    /// A sign-in or a key for `provider` from its page: the screen steps aside for the
    /// prompts and comes back on that provider once they are done.
    fn models_sign_in(&mut self, provider: String, key: bool) {
        let Some(local) = self.backend.local() else {
            if let Some(screen) = self.models_screen() {
                screen.say(
                    Role::Warning,
                    "this session runs elsewhere: sign in or add the key on the machine it runs on (azazel login)",
                );
            }
            return;
        };
        let app = local.app.clone();
        let Some(ui) = app.registry.services.get::<Ui>() else {
            return;
        };
        let config = crate::models::providers(&app.config)
            .unwrap_or_default()
            .into_iter()
            .find(|config| config.id == provider);
        let Some(config) = config else {
            return;
        };
        self.models_parked = self.overlay.take();
        let done = self.internal_tx.clone();
        tokio::spawn(async move {
            let result = match (key, &config.oauth) {
                (false, Some(settings)) => {
                    crate::login::sign_in_from_ui(
                        &app.credentials,
                        &config,
                        settings,
                        &Default::default(),
                        ui.clone(),
                        None,
                    )
                    .await
                }
                (false, None) => Err(format!("{provider} has no sign-in")),
                (true, _) => {
                    let prompt = format!("API key for {provider} (stored in ~/.azazel/auth.json)");
                    match ui.0.secret(&prompt).await {
                        Ok(typed) => {
                            crate::login::store_key(&app.credentials, &config, &typed, None).map(Some)
                        }
                        Err(_) => Ok(None),
                    }
                }
            };
            let _ = done.send(Internal::Models(Update::SignedIn { provider, result }));
        });
    }

    pub(super) async fn on_models(&mut self, update: Update) {
        match update {
            Update::Refreshed { provider, result } => {
                if let Some(screen) = self.models_screen() {
                    screen.refreshed(&provider, result);
                }
            }
            Update::SignedIn { provider, result } => {
                let Some(mut overlay) = self.models_parked.take() else {
                    return;
                };
                if let Some(local) = self.backend.local() {
                    local.account_changed();
                }
                let signed = matches!(result, Ok(Some(_)));
                let (data, _) = self.models_data().await;
                if let Kind::Models(screen) = &mut overlay.kind {
                    if signed {
                        screen.reload(data, Some(&provider));
                    }
                    match &result {
                        Ok(Some(line)) => screen.say(Role::Success, line),
                        Ok(None) => {}
                        Err(why) => screen.say(Role::Error, why),
                    }
                    for provider in screen.start_refreshes() {
                        self.models_asked.insert(provider.clone());
                        self.backend.refresh_models(provider, self.internal_tx.clone());
                    }
                }
                // Back once nothing else asks for the user.
                self.deferred.push_front(overlay);
            }
        }
    }

    /// Moves the open screen's spinner on, and asks a provider the selection rested on for
    /// its models.
    pub(super) fn models_tick(&mut self) {
        let frame = (azazel_ai::credentials::now_ms() / 80) as usize;
        let due = match &mut self.overlay {
            Some(Overlay {
                kind: Kind::Models(screen),
                ..
            }) => screen.tick(Instant::now(), frame),
            _ => None,
        };
        if let Some(provider) = due {
            self.models_asked.insert(provider.clone());
            self.backend.refresh_models(provider, self.internal_tx.clone());
        }
    }
}
