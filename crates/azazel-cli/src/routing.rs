//! Service tiers (`/fast`, `/slow`, `--tier`) and model presets (`/preset`, `--preset`).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use azazel_agent::{AgentCmd, AgentHandle};
use azazel_ai::{ModelRef, ModelSpec, Router, Tier};
use azazel_core::config::PresetConfig;
use azazel_core::{
    Command, CommandHandler, CommandOutcome, Config, EntryKind, ExtCtx, ExtError, Extension, Registrar,
    SessionStore,
};
use azazel_team::Team;

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

/// The roles of preset `name`, or which presets there are.
fn preset<'a>(presets: &'a BTreeMap<String, PresetConfig>, name: &str) -> Result<&'a PresetConfig, String> {
    presets.get(name).ok_or_else(|| match presets.is_empty() {
        true => {
            format!("no preset `{name}`: none is saved yet (/preset save <name> keeps the current models)")
        }
        false => format!(
            "no preset `{name}`; there are: {}",
            presets.keys().cloned().collect::<Vec<_>>().join(", ")
        ),
    })
}

/// `--preset`: the preset's roles replace the configured ones for this run.
pub fn launch_preset(config: &mut Config, name: &str) -> Result<(), String> {
    let roles = preset(&config.presets, name)?.roles.clone();
    config.roles.extend(roles);
    Ok(())
}

fn chain_label(chain: &[ModelSpec]) -> String {
    chain
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

struct PresetCommand {
    /// The configured presets plus the ones saved in this process.
    presets: Mutex<BTreeMap<String, PresetConfig>>,
    home_config: PathBuf,
}

impl PresetCommand {
    fn presets(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, PresetConfig>> {
        self.presets.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn list(&self) -> String {
        let presets = self.presets();
        if presets.is_empty() {
            return "no preset is saved yet: /preset save <name> keeps the current models".to_string();
        }
        let mut out = String::new();
        for (name, preset) in presets.iter() {
            out.push_str(name);
            out.push('\n');
            for (role, chain) in &preset.roles {
                out.push_str(&format!("  {role:<11} {}\n", chain_label(chain)));
            }
        }
        out.push_str("/preset <name> applies one · /preset save <name> keeps the current models");
        out
    }

    /// Points every role of the preset at its models. The lead goes back to following its role
    /// when the preset says what the lead or the default model is, so a model picked by hand
    /// does not outlive it.
    fn apply(&self, name: &str, ctx: &ExtCtx) -> Result<String, String> {
        let Some(router) = ctx.services.get::<Router>() else {
            return Err("no model router in this session".to_string());
        };
        let roles = preset(&self.presets(), name)?.roles.clone();
        let lead = ctx.services.get::<Team>().and_then(|team| team.lead());
        let session = ctx.services.get::<SessionStore>();
        let lead = lead.as_ref().map(|lead| (lead, session.as_deref()));
        Ok(format!("preset {name}: {}", apply_roles(&router, &roles, lead)))
    }

    /// Writes the roles as they are now under `[presets.<name>.roles]`, replacing a preset of
    /// that name.
    fn save(&self, name: &str, ctx: &ExtCtx) -> Result<String, String> {
        let Some(router) = ctx.services.get::<Router>() else {
            return Err("no model router in this session".to_string());
        };
        let roles = router.roles();
        save_roles(&self.home_config, name, &roles)?;
        let count = roles.len();
        self.presets().insert(name.to_string(), PresetConfig { roles });
        Ok(format!(
            "preset {name} saved with the {count} roles as they are now (in {})",
            self.home_config.display()
        ))
    }
}

#[async_trait]
impl CommandHandler for PresetCommand {
    async fn run(&self, args: &str, ctx: &ExtCtx) -> CommandOutcome {
        let mut words = args.split_whitespace();
        let done = match (words.next(), words.next(), words.next()) {
            (None, ..) => Ok(self.list()),
            (Some("save"), Some(name), None) => self.save(name, ctx),
            (Some("save"), ..) => Err("usage: /preset save <name>".to_string()),
            (Some(name), None, _) => self.apply(name, ctx),
            _ => Err("usage: /preset [name] or /preset save <name>".to_string()),
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
/// Returns what was done, e.g. `3 roles set; the lead now runs on provider/model`.
pub(crate) fn apply_roles(
    router: &Router,
    roles: &BTreeMap<String, Vec<ModelSpec>>,
    lead: Option<(&AgentHandle, Option<&SessionStore>)>,
) -> String {
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

/// Whether `name` can name a preset or a role: letters, digits, `-` and `_`, starting with a
/// letter.
pub(crate) fn plain_name(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

/// `Err` when `name` cannot name a preset.
pub(crate) fn preset_name(name: &str) -> Result<(), String> {
    match plain_name(name) && name != "save" {
        true => Ok(()),
        false => Err(format!(
            "`{name}` cannot name a preset: use letters, digits, `-` and `_`, starting with a letter"
        )),
    }
}

/// Writes `roles` as preset `name` in the config file `home_config`, replacing a preset of
/// that name.
pub(crate) fn save_roles(
    home_config: &std::path::Path,
    name: &str,
    roles: &BTreeMap<String, Vec<ModelSpec>>,
) -> Result<(), String> {
    preset_name(name)?;
    let write = |path: &[&str], value| {
        crate::init::set_config_value(home_config, path, value).map_err(|e| format!("{e:#}"))
    };
    write(&["presets", name], None)?;
    for (role, chain) in roles {
        let models = toml_edit::Array::from_iter(chain.iter().map(ToString::to_string));
        write(&["presets", name, "roles", role], Some(toml_edit::value(models)))?;
    }
    Ok(())
}

/// Registers `/fast`, `/slow` and `/preset`.
pub struct RoutingExt {
    pub presets: BTreeMap<String, PresetConfig>,
    pub home_config: PathBuf,
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
        r.command(Command {
            name: "preset".to_string(),
            description: "List the saved sets of models, apply one, or save the current one".to_string(),
            argument_hint: Some("[name | save <name>]".to_string()),
            handler: Arc::new(PresetCommand {
                presets: Mutex::new(self.presets.clone()),
                home_config: self.home_config.clone(),
            }),
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use azazel_agent::Services;
    use azazel_ai::{Catalog, RetryPolicy};

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

    #[tokio::test]
    async fn a_preset_is_saved_listed_and_applied() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, router) = context(&[
            ("default", &["acme/big"]),
            ("lead", &["@default"]),
            ("smol", &["acme/small", "@default"]),
        ]);
        let command = PresetCommand {
            presets: Mutex::default(),
            home_config: dir.path().join("config.toml"),
        };
        assert!(
            show(&command, "", &ctx)
                .await
                .unwrap()
                .starts_with("no preset is saved yet")
        );
        assert!(
            show(&command, "cheap", &ctx)
                .await
                .unwrap_err()
                .contains("none is saved yet")
        );
        assert!(show(&command, "save", &ctx).await.is_err());
        assert!(show(&command, "save a/b", &ctx).await.is_err());

        let saved = show(&command, "save big", &ctx).await.unwrap();
        assert!(saved.starts_with("preset big saved with the 3 roles"), "{saved}");
        // What was written is configuration that loads.
        let written = std::fs::read_to_string(&command.home_config).unwrap();
        let table: toml::Table = written.parse().unwrap();
        let loaded: PresetConfig = table["presets"]["big"].clone().try_into().unwrap();
        assert_eq!(loaded.roles["smol"], chain(&["acme/small", "@default"]));

        router.set_role("default", chain(&["acme/tiny"]));
        router.set_role("smol", chain(&["acme/tiny"]));
        show(&command, "save small", &ctx).await.unwrap();
        let listed = show(&command, "", &ctx).await.unwrap();
        assert!(
            listed.contains(
                "big\n  default     acme/big\n  lead        @default\n  smol        acme/small, @default\n"
            ),
            "{listed}"
        );

        let applied = show(&command, "big", &ctx).await.unwrap();
        assert_eq!(applied, "preset big: 3 roles set; the lead now runs on acme/big");
        assert_eq!(router.roles()["smol"], chain(&["acme/small", "@default"]));
        assert_eq!(
            show(&command, "nope", &ctx).await.unwrap_err(),
            "no preset `nope`; there are: big, small"
        );

        // Saving under a used name replaces the preset, role for role.
        router.set_role("default", chain(&["acme/other"]));
        show(&command, "save big", &ctx).await.unwrap();
        let table: toml::Table = std::fs::read_to_string(&command.home_config)
            .unwrap()
            .parse()
            .unwrap();
        let loaded: PresetConfig = table["presets"]["big"].clone().try_into().unwrap();
        assert_eq!(loaded.roles["default"], chain(&["acme/other"]));
        assert!(table["presets"].get("small").is_some());
    }

    #[test]
    fn a_preset_named_at_launch_replaces_the_configured_roles() {
        let mut config = Config::default();
        config.presets.insert(
            "cheap".to_string(),
            PresetConfig {
                roles: BTreeMap::from([("default".to_string(), chain(&["acme/small"]))]),
            },
        );
        launch_preset(&mut config, "cheap").unwrap();
        assert_eq!(config.roles["default"], chain(&["acme/small"]));
        assert_eq!(
            launch_preset(&mut config, "dear").unwrap_err(),
            "no preset `dear`; there are: cheap"
        );
    }
}
