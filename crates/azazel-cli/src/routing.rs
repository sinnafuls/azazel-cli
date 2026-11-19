//! Service tiers (`/fast`, `/slow`, `--tier`) and the command that switches profiles (`/profile`).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use azazel_agent::{AgentCmd, AgentHandle};
use azazel_ai::{ModelRef, ModelSpec, Router, Tier};
use azazel_core::{
    Command, CommandHandler, CommandOutcome, EntryKind, ExtCtx, ExtError, Extension, Registrar, SessionStore,
};
use azazel_team::Team;

use crate::profiles::{Book, ProfileSource, Reason};

/// The tier as the status line and the commands call it.
pub fn tier_word(tier: Tier) -> &'static str {
    match tier {
        Tier::Priority => "fast",
        Tier::Standard => "standard",
        Tier::Economy => "slow",
    }
}

/// `--tier`: `standard` is the same as naming none.
pub fn parse_tier(text: &str) -> Result<Option<Tier>, String> {
    let tier: Tier = text.parse().map_err(|e| format!("{e}"))?;
    Ok(Some(tier).filter(|tier| *tier != Tier::Standard))
}

/// What the transcript says when a reply is given up and its model asked again: after a stall
/// (`stalled_secs` of silence) or any other failure.
pub fn retry_line(model: &str, attempt: u32, stalled_secs: Option<u64>) -> String {
    match stalled_secs {
        Some(secs) => format!("no answer from {model} for {secs} s; retrying (attempt {attempt})"),
        None => format!("{model} did not answer; retrying (attempt {attempt})"),
    }
}

/// What the transcript says, once, when a model refuses the tier.
pub fn tier_dropped_line(model: &ModelRef, tier: Tier) -> String {
    format!(
        "{model} does not take the {} tier; it answers at the standard tier for the rest of this session",
        tier.as_str()
    )
}

/// The line `/cost` adds while a tier is on.
pub fn tier_cost_note(tier: Tier) -> String {
    format!(
        "note: the {} tier is on, and tier prices are not in the catalogue: the figure is at standard rates",
        tier.as_str()
    )
}

/// What turning a tier on means, in one line.
fn tier_effect(tier: Tier) -> &'static str {
    match tier {
        Tier::Priority => {
            "requests ask for the priority tier: faster output at a higher price on OpenAI models \
             (priority processing) and Claude models (fast mode); other models ignore it"
        }
        Tier::Economy => {
            "requests ask for the economy tier: cheaper and slower on OpenAI models (flex processing); \
             other models ignore it"
        }
        Tier::Standard => "requests run at each provider's standard tier",
    }
}

/// `/fast` and `/slow`: one command per tier.
struct TierCommand(Tier);

#[async_trait]
impl CommandHandler for TierCommand {
    async fn run(&self, args: &str, ctx: &ExtCtx) -> CommandOutcome {
        let name = tier_word(self.0);
        let Some(router) = ctx.services.get::<Router>() else {
            return CommandOutcome::Error("no model router in this session".to_string());
        };
        let on = router.tier() == Some(self.0);
        let turn_on = match args.trim() {
            "" => !on,
            "on" => true,
            "off" => false,
            "status" => {
                let mut line = match router.tier() {
                    Some(tier) => format!("{}: on ({})", tier_word(tier), tier_effect(tier)),
                    None => format!("{name}: off ({})", tier_effect(Tier::Standard)),
                };
                let model = ctx
                    .services
                    .get::<Team>()
                    .and_then(|team| team.lead())
                    .and_then(|lead| lead.status().model);
                if let Some(model) = model.filter(|model| router.is_tierless(model)) {
                    line.push_str(&format!(
                        "\n{model} refused the tier and answers at the standard tier"
                    ));
                }
                return CommandOutcome::Show(line);
            }
            other => {
                return CommandOutcome::Error(format!("usage: /{name} [on|off|status], not `{other}`"));
            }
        };
        match (turn_on, on) {
            (true, _) => {
                router.set_tier(Some(self.0));
                CommandOutcome::Show(format!("{name}: on. From now on {}.", tier_effect(self.0)))
            }
            (false, true) => {
                router.set_tier(None);
                CommandOutcome::Show(format!("{name}: off. Back to the standard tier."))
            }
            // Turning one tier off leaves the other one alone.
            (false, false) => CommandOutcome::Show(format!("{name} was not on")),
        }
    }
}

fn chain_label(chain: &[ModelSpec]) -> String {
    chain
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// How a profile's source reads in a list.
fn source_label(source: ProfileSource) -> &'static str {
    match source {
        ProfileSource::Builtin => "built-in",
        ProfileSource::User => "yours",
        ProfileSource::Project => "project",
    }
}

/// What a profile needs that is not there yet, as one sentence; `None` when it can be used.
pub(crate) fn needs_login(missing: &[String]) -> Option<String> {
    (!missing.is_empty()).then(|| {
        let logins: Vec<String> = missing.iter().map(|id| format!("`azazel login {id}`")).collect();
        format!("needs a login: run {}", logins.join(" and "))
    })
}

/// `/profile` (and `/preset`, its old name).
struct ProfileCommand {
    book: Arc<Book>,
    home_config: PathBuf,
}

impl ProfileCommand {
    fn list(&self) -> String {
        let mut out = String::new();
        for profile in self.book.list() {
            let usable = self.book.usability(&profile);
            out.push_str(&format!("{} ({})", profile.name, source_label(profile.source)));
            if let Some(why) = usable.problem() {
                out.push_str(&format!(" - {why}"));
            }
            out.push('\n');
            for role in ["default", "slow", "smol"] {
                if let Some(chain) = profile.roles.get(role) {
                    out.push_str(&format!("  {role:<11} {}\n", chain_label(chain)));
                }
            }
        }
        out.push_str("/profile <name> applies one · /profile save <name> keeps the current models");
        out
    }

    /// Switches to the profile, unless a provider it names has no login.
    fn apply(&self, name: &str, ctx: &ExtCtx) -> Result<String, String> {
        let Some(router) = ctx.services.get::<Router>() else {
            return Err("no model router in this session".to_string());
        };
        let team = ctx.services.get::<Team>();
        let lead = team.as_ref().and_then(|team| team.lead());
        let session = ctx.services.get::<SessionStore>();
        let lead = lead.as_ref().map(|lead| (lead, session.as_deref()));
        self.book
            .switch(name, &router, lead, team.as_deref(), Reason::User)
    }

    /// Writes the roles as they are now under `[profiles.<name>.roles]`, replacing a profile of
    /// that name.
    fn save(&self, name: &str, ctx: &ExtCtx) -> Result<String, String> {
        let Some(router) = ctx.services.get::<Router>() else {
            return Err("no model router in this session".to_string());
        };
        let roles = router.roles();
        self.book.save(&self.home_config, name, &roles)?;
        Ok(format!(
            "profile {name} saved with the {} roles as they are now (in {})",
            roles.len(),
            self.home_config.display()
        ))
    }
}

#[async_trait]
impl CommandHandler for ProfileCommand {
    async fn run(&self, args: &str, ctx: &ExtCtx) -> CommandOutcome {
        let mut words = args.split_whitespace();
        let done = match (words.next(), words.next(), words.next()) {
            (None, ..) => Ok(self.list()),
            (Some("save"), Some(name), None) => self.save(name, ctx),
            (Some("save"), ..) => Err("usage: /profile save <name>".to_string()),
            (Some(name), None, _) => self.apply(name, ctx),
            _ => Err("usage: /profile [name] or /profile save <name>".to_string()),
        };
        match done {
            Ok(line) => CommandOutcome::Show(line),
            Err(why) => CommandOutcome::Error(why),
        }
    }
}

/// Points every role in `roles` at its chain for the rest of the process. When the roles say
/// what the lead or the default model is, the lead goes back to following its role, so a model
/// picked by hand does not outlive them; the change is recorded in `session` when given.
/// Sub-agents that are running keep the models they resolve to now (`team`), so a switch never
/// moves one in the middle of its task; agents started later use the new roles.
/// Returns what was done, e.g. `3 roles set; the lead now runs on provider/model`.
pub(crate) fn apply_roles(
    router: &Router,
    roles: &BTreeMap<String, Vec<ModelSpec>>,
    lead: Option<(&AgentHandle, Option<&SessionStore>)>,
    team: Option<&Team>,
) -> String {
    if let Some(team) = team {
        team.pin_running();
    }
    for (role, chain) in roles {
        router.set_role(role, chain.clone());
    }
    let mut line = match roles.len() {
        1 => "1 role set".to_string(),
        count => format!("{count} roles set"),
    };
    if roles.contains_key("lead") || roles.contains_key("default") {
        let chain = vec![ModelSpec::Role("lead".to_string())];
        if let Some((lead, session)) = lead {
            follow_role(lead, session);
        }
        if let Some(model) = router.resolve(&chain).first() {
            line.push_str(&format!("; the lead now runs on {model}"));
        }
    }
    line
}

/// Puts the lead on `@lead` (unless it is there already), recorded in `session` so a resumed
/// session comes back on it.
pub(crate) fn follow_role(lead: &AgentHandle, session: Option<&SessionStore>) {
    let chain = vec![ModelSpec::Role("lead".to_string())];
    if lead.status().chain == chain {
        return;
    }
    lead.send(AgentCmd::SetModel(chain.clone()));
    if let Some(session) = session {
        session.append(EntryKind::ModelChange { chain });
    }
}

/// Whether `name` can name a profile or a role: letters, digits, `-` and `_`, starting with a
/// letter.
pub(crate) fn plain_name(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

/// `Err` when `name` cannot name a profile. `save` is the word of `/profile save`, and `on_limit`
/// and `on_reset` are keys of `[profiles]`.
pub(crate) fn profile_name(name: &str) -> Result<(), String> {
    match plain_name(name) && !matches!(name, "save" | "on_limit" | "on_reset") {
        true => Ok(()),
        false => Err(format!(
            "`{name}` cannot name a profile: use letters, digits, `-` and `_`, starting with a letter              (not save, on_limit or on_reset)"
        )),
    }
}

/// Writes `roles` as profile `name` in the config file `home_config`, replacing a profile of
/// that name. A preset of that name (the old table) is left alone; the profile wins over it.
pub(crate) fn save_roles(
    home_config: &std::path::Path,
    name: &str,
    roles: &BTreeMap<String, Vec<ModelSpec>>,
) -> Result<(), String> {
    profile_name(name)?;
    // One table, written in one go: a failure leaves the old profile as it was.
    let mut table = toml_edit::Table::new();
    let mut by_role = toml_edit::Table::new();
    for (role, chain) in roles {
        let models = toml_edit::Array::from_iter(chain.iter().map(ToString::to_string));
        by_role.insert(role, toml_edit::value(models));
    }
    table.insert("roles", toml_edit::Item::Table(by_role));
    crate::init::set_config_value(
        home_config,
        &["profiles", name],
        Some(toml_edit::Item::Table(table)),
    )
    .map_err(|e| format!("{e:#}"))
}

/// Registers `/fast`, `/slow` and `/profile` (and `/preset`, its old name).
pub struct RoutingExt {
    /// The profiles and which of them can be used; also a service for RPC and the terminal.
    pub book: Arc<Book>,
    pub home_config: PathBuf,
    /// `[profiles] on_limit` and `on_reset`.
    pub on_limit: String,
    pub on_reset: azazel_core::config::ProfileReset,
    /// `--profile`: the profile the run starts on.
    pub started_on: Option<String>,
}

impl Extension for RoutingExt {
    fn id(&self) -> &str {
        "routing"
    }

    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        for (tier, description) in [
            (
                Tier::Priority,
                "Faster replies at a higher price (the priority tier)",
            ),
            (Tier::Economy, "Cheaper, slower replies (the economy tier)"),
        ] {
            r.command(Command {
                name: tier_word(tier).to_string(),
                description: description.to_string(),
                argument_hint: Some("[on|off|status]".to_string()),
                handler: Arc::new(TierCommand(tier)),
            });
        }
        let switcher = Arc::new(crate::profiles::limit::ProfileSwitcher::new(
            &self.on_limit,
            self.on_reset,
            self.book.list(),
            self.book.access_fn(),
            r.services().clone(),
            self.started_on.clone(),
        ));
        r.service(switcher.clone());
        r.on(
            azazel_core::EventKind::UserPrompt,
            Arc::new(crate::profiles::limit::AtPrompt(switcher)),
        );
        r.service(self.book.clone());
        let profile = Arc::new(ProfileCommand {
            book: self.book.clone(),
            home_config: self.home_config.clone(),
        });
        for (name, description) in [
            (
                "profile",
                "List the profiles (sets of models), apply one, or save the current one",
            ),
            ("preset", "The old name of /profile"),
        ] {
            r.command(Command {
                name: name.to_string(),
                description: description.to_string(),
                argument_hint: Some("[name | save <name>]".to_string()),
                handler: profile.clone(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use azazel_agent::Services;
    use azazel_ai::{Catalog, RetryPolicy};
    use azazel_core::config::ProfileConfig;

    use super::*;

    fn chain(specs: &[&str]) -> Vec<ModelSpec> {
        specs.iter().map(|spec| spec.parse().unwrap()).collect()
    }

    fn context(roles: &[(&str, &[&str])]) -> (ExtCtx, Arc<Router>) {
        let roles = roles
            .iter()
            .map(|(role, specs)| (role.to_string(), chain(specs)))
            .collect();
        let router = Arc::new(Router::new(
            Vec::new(),
            Catalog::default(),
            roles,
            RetryPolicy::default(),
        ));
        let services = Arc::new(Services::new());
        services.insert(router.clone());
        let ctx = ExtCtx {
            services,
            cwd: PathBuf::from("."),
            session_id: String::new(),
        };
        (ctx, router)
    }

    async fn show(handler: &dyn CommandHandler, args: &str, ctx: &ExtCtx) -> Result<String, String> {
        match handler.run(args, ctx).await {
            CommandOutcome::Show(line) => Ok(line),
            CommandOutcome::Error(why) => Err(why),
            _ => panic!("the command neither showed nor failed"),
        }
    }

    #[tokio::test]
    async fn fast_and_slow_set_the_tier_requests_ask_for() {
        let (ctx, router) = context(&[]);
        let (fast, slow) = (TierCommand(Tier::Priority), TierCommand(Tier::Economy));

        assert!(
            show(&fast, "status", &ctx)
                .await
                .unwrap()
                .starts_with("fast: off")
        );
        assert!(show(&fast, "on", &ctx).await.unwrap().starts_with("fast: on."));
        assert_eq!(router.tier(), Some(Tier::Priority));
        assert!(show(&slow, "status", &ctx).await.unwrap().starts_with("fast: on"));
        // Turning the other tier off changes nothing; turning it on replaces this one.
        assert_eq!(show(&slow, "off", &ctx).await.unwrap(), "slow was not on");
        assert_eq!(router.tier(), Some(Tier::Priority));
        show(&slow, "", &ctx).await.unwrap();
        assert_eq!(router.tier(), Some(Tier::Economy));
        show(&slow, "", &ctx).await.unwrap();
        assert_eq!(router.tier(), None);
        assert!(
            show(&fast, "maybe", &ctx)
                .await
                .unwrap_err()
                .starts_with("usage: /fast")
        );

        assert_eq!(parse_tier("priority"), Ok(Some(Tier::Priority)));
        assert_eq!(parse_tier("standard"), Ok(None));
        assert!(
            parse_tier("turbo")
                .unwrap_err()
                .contains("priority, standard or economy")
        );
    }

    fn command(dir: &std::path::Path, access: fn(&str) -> crate::profiles::Access) -> ProfileCommand {
        ProfileCommand {
            book: Arc::new(Book::new(crate::profiles::builtin(), Arc::new(access), None)),
            home_config: dir.join("config.toml"),
        }
    }

    #[tokio::test]
    async fn a_profile_is_saved_listed_and_applied() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, router) = context(&[
            ("default", &["acme/big"]),
            ("lead", &["@default"]),
            ("smol", &["acme/small", "@default"]),
        ]);
        let command = command(dir.path(), |id| match id {
            "openai-codex" => crate::profiles::Access::NoLogin,
            _ => crate::profiles::Access::Ready,
        });
        let listed = show(&command, "", &ctx).await.unwrap();
        assert!(
            listed.contains("claude (built-in)\n  default     anthropic/claude-opus-5-5\n"),
            "{listed}"
        );
        assert!(
            listed.contains("gpt (built-in) - needs a login: run `azazel login openai-codex`"),
            "{listed}"
        );
        // A profile that needs a login is refused, and nothing changes.
        let refused = show(&command, "gpt", &ctx).await.unwrap_err();
        assert!(refused.contains("azazel login openai-codex"), "{refused}");
        assert_eq!(router.roles()["default"], chain(&["acme/big"]));
        assert!(show(&command, "save", &ctx).await.is_err());
        assert!(show(&command, "save a/b", &ctx).await.is_err());
        assert!(show(&command, "save on_limit", &ctx).await.is_err());

        let saved = show(&command, "save big", &ctx).await.unwrap();
        assert!(saved.starts_with("profile big saved with the 3 roles"), "{saved}");
        // What was written is configuration that loads.
        let written = std::fs::read_to_string(&command.home_config).unwrap();
        let table: toml::Table = written.parse().unwrap();
        let loaded: ProfileConfig = table["profiles"]["big"].clone().try_into().unwrap();
        assert_eq!(loaded.roles["smol"], chain(&["acme/small", "@default"]));

        router.set_role("default", chain(&["acme/tiny"]));
        router.set_role("smol", chain(&["acme/tiny"]));
        show(&command, "save small", &ctx).await.unwrap();
        let listed = show(&command, "", &ctx).await.unwrap();
        assert!(
            listed.contains("big (yours)\n  default     acme/big\n  smol        acme/small, @default\n"),
            "{listed}"
        );

        let applied = show(&command, "big", &ctx).await.unwrap();
        assert_eq!(applied, "profile big: 3 roles set; the lead now runs on acme/big");
        assert_eq!(router.roles()["smol"], chain(&["acme/small", "@default"]));
        let error = show(&command, "nope", &ctx).await.unwrap_err();
        assert!(
            error.starts_with("no profile `nope`; there are: claude, gpt,"),
            "{error}"
        );
        assert!(error.ends_with("big, small"), "{error}");

        // Saving under a used name replaces the profile, role for role.
        router.set_role("default", chain(&["acme/other"]));
        show(&command, "save big", &ctx).await.unwrap();
        let table: toml::Table = std::fs::read_to_string(&command.home_config)
            .unwrap()
            .parse()
            .unwrap();
        let loaded: ProfileConfig = table["profiles"]["big"].clone().try_into().unwrap();
        assert_eq!(loaded.roles["default"], chain(&["acme/other"]));
        assert!(table["profiles"].get("small").is_some());
    }

    #[tokio::test]
    async fn a_profile_naming_a_provider_nobody_has_is_listed_unusable_and_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, router) = context(&[("default", &["acme/big"]), ("lead", &["@default"])]);
        let user = |name: &str, models: &[&str]| crate::profiles::Profile {
            name: name.to_string(),
            source: crate::profiles::ProfileSource::User,
            roles: [("default".to_string(), chain(models))].into(),
        };
        let credentials = Arc::new(azazel_ai::Credentials::new(dir.path().join("auth.json")));
        // `acme` (an extension's) and `mock` are registered providers without an HTTP endpoint.
        let access =
            crate::profiles::access(Vec::new(), credentials, |id| Some(matches!(id, "acme" | "mock")));
        let command = ProfileCommand {
            book: Arc::new(Book::new(
                vec![
                    user("x", &["anthropc/claude-x"]),
                    user("ext", &["acme/a", "mock/lead"]),
                ],
                Arc::new(access),
                None,
            )),
            home_config: dir.path().join("config.toml"),
        };
        let listed = show(&command, "", &ctx).await.unwrap();
        assert!(
            listed.contains("x (yours) - names an unknown provider `anthropc`"),
            "{listed}"
        );
        assert!(listed.contains("ext (yours)\n"), "{listed}");
        let refused = show(&command, "x", &ctx).await.unwrap_err();
        assert!(refused.contains("unknown provider `anthropc`"), "{refused}");
        assert_eq!(router.roles()["default"], chain(&["acme/big"]));
        show(&command, "ext", &ctx).await.unwrap();
        assert_eq!(router.roles()["default"], chain(&["acme/a", "mock/lead"]));
    }
}
