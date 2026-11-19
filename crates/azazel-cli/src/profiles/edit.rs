//! Changes to the profiles in the home config: what the Profiles page of `/models` and
//! `/profile save` write. An [`Edit`] is what the user asked for; [`plan`] checks it against the
//! profiles there are and says what is to be written as a [`Change`], which [`write_doc`] puts
//! into the config file and [`write_state`] into the profiles held in memory, so that the two
//! cannot disagree.

use std::collections::BTreeMap;

use azazel_ai::ModelSpec;
use azazel_core::config::BUILTIN_PROFILES;
use toml_edit::{Array, DocumentMut, Item, Table, TableLike};

use super::{Profile, ProfileSource, builtin};

pub type Roles = BTreeMap<String, Vec<ModelSpec>>;

/// What a user asks of a profile, with names and chains as typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    /// A new profile from `from`'s roles, or from the roles of the session when `None`.
    New {
        name: String,
        from: Option<String>,
    },
    Rename {
        from: String,
        to: String,
    },
    /// Removes a saved profile, or the override of a built-in one (it is back to its own).
    Delete(String),
    /// Sets one role of a profile to `chain`, or takes the role out of it when `None`. On a
    /// built-in profile this writes an override of the whole profile.
    SetChain {
        name: String,
        role: String,
        chain: Option<Vec<String>>,
    },
    /// Switches a built-in profile off.
    Disable(String),
    Enable(String),
}

/// What is written for an [`Edit`].
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// The profile whole: a new one, a built-in's override, or a saved one's new roles.
    Write {
        name: String,
        roles: Roles,
    },
    Rename {
        from: String,
        to: String,
    },
    /// Takes the profile's table out.
    Remove {
        name: String,
    },
    Disable {
        name: String,
    },
    Enable {
        name: String,
    },
}

/// The profiles and settings an edit is checked against.
pub struct Known<'a> {
    /// Every profile, disabled ones too.
    pub profiles: &'a [Profile],
    pub disabled: &'a [String],
    /// `[profiles] on_limit`: the profile that must stay.
    pub on_limit: &'a str,
    /// The roles of the session, for a profile made from them.
    pub current: &'a Roles,
}

fn is_builtin(name: &str) -> bool {
    BUILTIN_PROFILES.contains(&name)
}

/// Checks `edit` and says what to write.
pub fn plan(known: &Known<'_>, edit: &Edit) -> Result<Change, String> {
    let find = |name: &str| {
        known
            .profiles
            .iter()
            .find(|profile| profile.name == name)
            .ok_or_else(|| format!("there is no profile {name}"))
    };
    let editable = |profile: &Profile| match profile.source {
        ProfileSource::Project => Err(format!(
            "profile {} comes from the project's config: change it there",
            profile.name
        )),
        _ => Ok(()),
    };
    let not_on_limit = |name: &str, what: &str| match known.on_limit == name {
        true => Err(format!(
            "profile {name} is the one used on a usage limit (profiles.on_limit): change that before {what}"
        )),
        false => Ok(()),
    };
    match edit {
        Edit::New { name, from } => {
            crate::routing::profile_name(name)?;
            if known.profiles.iter().any(|profile| profile.name == *name) {
                return Err(format!("there is a profile {name} already"));
            }
            let roles = match from {
                Some(from) => find(from)?.roles.clone(),
                None => known.current.clone(),
            };
            if roles.is_empty() {
                return Err("there are no roles to make a profile of".to_string());
            }
            Ok(Change::Write {
                name: name.clone(),
                roles,
            })
        }
        Edit::Rename { from, to } => {
            let profile = find(from)?;
            if is_builtin(from) {
                return Err(format!(
                    "{from} is built in and keeps its name; copy it to get one of your own"
                ));
            }
            editable(profile)?;
            not_on_limit(from, "renaming it")?;
            crate::routing::profile_name(to)?;
            if known.profiles.iter().any(|profile| profile.name == *to) {
                return Err(format!("there is a profile {to} already"));
            }
            Ok(Change::Rename {
                from: from.clone(),
                to: to.clone(),
            })
        }
        Edit::Delete(name) => {
            let profile = find(name)?;
            editable(profile)?;
            match (is_builtin(name), profile.source) {
                (true, ProfileSource::Builtin) => Err(format!(
                    "{name} is built in and has nothing of yours to remove; disable it to hide it"
                )),
                (true, _) => Ok(Change::Remove { name: name.clone() }),
                (false, _) => {
                    not_on_limit(name, "deleting it")?;
                    Ok(Change::Remove { name: name.clone() })
                }
            }
        }
        Edit::SetChain { name, role, chain } => {
            let profile = find(name)?;
            editable(profile)?;
            let mut roles = profile.roles.clone();
            match chain.as_deref().filter(|chain| !chain.is_empty()) {
                Some(chain) => {
                    roles.insert(role.clone(), crate::roles::parse_chain(chain)?);
                }
                None => {
                    roles.remove(role);
                }
            }
            if roles.is_empty() {
                return Err(format!(
                    "profile {name} would have no role left: delete it instead"
                ));
            }
            Ok(Change::Write {
                name: name.clone(),
                roles,
            })
        }
        Edit::Disable(name) => {
            find(name)?;
            if !is_builtin(name) {
                return Err(format!("{name} is one you saved: delete it instead"));
            }
            if known.disabled.contains(name) {
                return Err(format!("profile {name} is disabled already"));
            }
            not_on_limit(name, "disabling it")?;
            Ok(Change::Disable { name: name.clone() })
        }
        Edit::Enable(name) => match known.disabled.contains(name) {
            true => Ok(Change::Enable { name: name.clone() }),
            false => Err(format!("profile {name} is not disabled")),
        },
    }
}

/// The table `[profiles]` of `doc`, made when there is none.
fn profiles_table(doc: &mut DocumentMut) -> Result<&mut dyn TableLike, String> {
    doc.as_table_mut()
        .entry("profiles")
        .or_insert_with(|| {
            let mut table = Table::new();
            table.set_implicit(true);
            Item::Table(table)
        })
        .as_table_like_mut()
        .ok_or_else(|| "`profiles` in the config is not a table".to_string())
}

fn disabled_names(table: &dyn TableLike) -> Vec<String> {
    table
        .get("disabled")
        .and_then(Item::as_array)
        .map(|array| {
            array
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn set_disabled(table: &mut dyn TableLike, names: Vec<String>) {
    match names.is_empty() {
        true => {
            table.remove("disabled");
        }
        false => {
            table.insert("disabled", toml_edit::value(Array::from_iter(names)));
        }
    }
}

/// Takes the old-named table (`[presets.<name>]`) of a profile out, when there is one.
fn take_preset(doc: &mut DocumentMut, name: &str) -> Option<Item> {
    doc.get_mut("presets")
        .and_then(Item::as_table_like_mut)
        .and_then(|presets| presets.remove(name))
}

/// What a change says to the user, and the profile whose roles it changed (none when the
/// profile's roles stay as they are).
pub fn describe(change: &Change) -> (String, Option<String>) {
    match change {
        Change::Write { name, .. } if is_builtin(name) => (
            format!(
                "{name} is saved as your own version of the built-in profile (reset it to get the built-in back)"
            ),
            Some(name.clone()),
        ),
        Change::Write { name, .. } => (format!("profile {name} saved"), Some(name.clone())),
        Change::Rename { from, to } => (format!("profile {from} is now {to}"), Some(from.clone())),
        Change::Remove { name } if is_builtin(name) => (
            format!("{name} is the built-in profile again"),
            Some(name.clone()),
        ),
        Change::Remove { name } => (format!("profile {name} deleted"), None),
        Change::Disable { name } => (format!("profile {name} is disabled"), None),
        Change::Enable { name } => (format!("profile {name} is enabled"), None),
    }
}

/// Writes `change` into the config text, keeping the rest as it is.
pub fn write_doc(doc: &mut DocumentMut, change: &Change) -> Result<(), String> {
    match change {
        Change::Write { name, roles } => {
            let mut by_role = Table::new();
            for (role, chain) in roles {
                let models = Array::from_iter(chain.iter().map(ToString::to_string));
                by_role.insert(role, toml_edit::value(models));
            }
            let mut table = Table::new();
            table.set_implicit(true);
            table.insert("roles", Item::Table(by_role));
            profiles_table(doc)?.insert(name, Item::Table(table));
        }
        Change::Rename { from, to } => {
            let moved = profiles_table(doc)?.remove(from);
            let old = take_preset(doc, from);
            let item = moved
                .or(old)
                .ok_or_else(|| format!("there is no saved profile {from} to rename"))?;
            let profiles = profiles_table(doc)?;
            let mut names = disabled_names(profiles);
            for name in names.iter_mut().filter(|name| *name == from) {
                name.clone_from(to);
            }
            profiles.insert(to, item);
            if !names.is_empty() {
                set_disabled(profiles, names);
            }
        }
        Change::Remove { name } => {
            let removed = profiles_table(doc)?.remove(name).is_some();
            let old = take_preset(doc, name).is_some();
            if !removed && !old {
                return Err(format!("there is no saved profile {name} in the config"));
            }
        }
        Change::Disable { name } => {
            let profiles = profiles_table(doc)?;
            let mut names = disabled_names(profiles);
            if !names.contains(name) {
                names.push(name.clone());
            }
            set_disabled(profiles, names);
        }
        Change::Enable { name } => {
            let profiles = profiles_table(doc)?;
            let mut names = disabled_names(profiles);
            names.retain(|n| n != name);
            set_disabled(profiles, names);
        }
    }
    Ok(())
}

/// Writes `change` into the profiles held in memory as [`write_doc`] wrote it to the file.
pub fn write_state(profiles: &mut Vec<Profile>, disabled: &mut Vec<String>, change: &Change) {
    match change {
        Change::Write { name, roles } => {
            let saved = Profile {
                name: name.clone(),
                source: ProfileSource::User,
                roles: roles.clone(),
            };
            match profiles.iter_mut().find(|profile| profile.name == *name) {
                Some(old) => *old = saved,
                None => profiles.push(saved),
            }
        }
        Change::Rename { from, to } => {
            if let Some(profile) = profiles.iter_mut().find(|profile| profile.name == *from) {
                profile.name.clone_from(to);
            }
            for name in disabled.iter_mut().filter(|name| *name == from) {
                name.clone_from(to);
            }
        }
        Change::Remove { name } => match builtin::all().into_iter().find(|built| built.name == *name) {
            Some(built) => {
                if let Some(profile) = profiles.iter_mut().find(|profile| profile.name == *name) {
                    *profile = built;
                }
            }
            None => profiles.retain(|profile| profile.name != *name),
        },
        Change::Disable { name } => {
            if !disabled.contains(name) {
                disabled.push(name.clone());
            }
        }
        Change::Enable { name } => disabled.retain(|n| n != name),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use azazel_core::config::ProfilesConfig;

    use super::*;

    fn chain(specs: &[&str]) -> Vec<ModelSpec> {
        specs.iter().map(|spec| spec.parse().unwrap()).collect()
    }

    fn roles(pairs: &[(&str, &str)]) -> Roles {
        pairs
            .iter()
            .map(|(role, model)| (role.to_string(), chain(&[model])))
            .collect()
    }

    fn saved(name: &str, model: &str) -> Profile {
        Profile {
            name: name.to_string(),
            source: ProfileSource::User,
            roles: roles(&[("default", model)]),
        }
    }

    struct World {
        profiles: Vec<Profile>,
        disabled: Vec<String>,
        on_limit: String,
        current: Roles,
        text: DocumentMut,
    }

    impl World {
        fn new() -> World {
            let mut profiles = builtin::all();
            profiles.push(saved("mine", "acme/mine"));
            World {
                profiles,
                disabled: Vec::new(),
                on_limit: String::new(),
                current: roles(&[("default", "acme/now"), ("smol", "acme/now-mini")]),
                text: "# my config\n[editor]\nmode = \"vim\"\n\n[profiles.mine.roles]\ndefault = [\"acme/mine\"]\n"
                    .parse()
                    .unwrap(),
            }
        }

        /// Plans, writes the text and the state, as the book does.
        fn run(&mut self, edit: Edit) -> Result<(), String> {
            let change = plan(
                &Known {
                    profiles: &self.profiles,
                    disabled: &self.disabled,
                    on_limit: &self.on_limit,
                    current: &self.current,
                },
                &edit,
            )?;
            write_doc(&mut self.text, &change)?;
            write_state(&mut self.profiles, &mut self.disabled, &change);
            Ok(())
        }

        /// The `[profiles]` and `[presets]` a load of the written text would give.
        fn loaded(
            &self,
        ) -> (
            ProfilesConfig,
            BTreeMap<String, azazel_core::config::ProfileConfig>,
        ) {
            #[derive(serde::Deserialize, Default)]
            #[serde(default)]
            struct Part {
                profiles: ProfilesConfig,
                presets: BTreeMap<String, azazel_core::config::ProfileConfig>,
            }
            let part: Part = toml::from_str(&self.text.to_string()).unwrap();
            (part.profiles, part.presets)
        }

        fn profiles_config(&self) -> ProfilesConfig {
            self.loaded().0
        }

        fn names(&self) -> Vec<String> {
            self.profiles.iter().map(|p| p.name.clone()).collect()
        }
    }

    fn new(name: &str, from: Option<&str>) -> Edit {
        Edit::New {
            name: name.to_string(),
            from: from.map(str::to_string),
        }
    }

    fn rename(from: &str, to: &str) -> Edit {
        Edit::Rename {
            from: from.to_string(),
            to: to.to_string(),
        }
    }

    fn delete(name: &str) -> Edit {
        Edit::Delete(name.to_string())
    }

    fn set(name: &str, role: &str, chain: Option<&[&str]>) -> Edit {
        Edit::SetChain {
            name: name.to_string(),
            role: role.to_string(),
            chain: chain.map(|chain| chain.iter().map(ToString::to_string).collect()),
        }
    }

    fn built_in(name: &str) -> Profile {
        builtin::all().into_iter().find(|p| p.name == name).unwrap()
    }

    #[test]
    fn a_new_profile_takes_the_roles_of_the_session_or_a_copy_of_another() {
        let mut world = World::new();
        world.run(new("now", None)).unwrap();
        world.run(new("twin", Some("cheap"))).unwrap();
        let config = world.profiles_config();
        assert_eq!(config.entries["now"].roles["smol"], chain(&["acme/now-mini"]));
        assert_eq!(config.entries["twin"].roles, built_in("cheap").roles);
        assert_eq!(world.profiles.last().unwrap().name, "twin");
        // The rest of the file is as it was.
        assert!(
            world
                .text
                .to_string()
                .starts_with("# my config\n[editor]\nmode = \"vim\"\n")
        );
        assert!(world.run(new("now", None)).unwrap_err().contains("already"));
        assert!(world.run(new("cheap", None)).unwrap_err().contains("already"));
        assert!(world.run(new("not ok", None)).is_err());
        assert!(world.run(new("disabled", Some("cheap"))).is_ok());
        assert!(world.run(new("save", None)).is_err());
        assert!(
            world
                .run(new("ghost", Some("nobody")))
                .unwrap_err()
                .contains("no profile")
        );
    }

    #[test]
    fn a_saved_profile_is_renamed_with_its_roles() {
        let mut world = World::new();
        world.run(rename("mine", "ours")).unwrap();
        let config = world.profiles_config();
        assert!(!config.entries.contains_key("mine"));
        assert_eq!(config.entries["ours"].roles["default"], chain(&["acme/mine"]));
        assert!(world.names().contains(&"ours".to_string()));
        assert!(
            world
                .run(rename("cheap", "frugal"))
                .unwrap_err()
                .contains("built in")
        );
        assert!(world.run(rename("ours", "gpt")).unwrap_err().contains("already"));
        assert!(world.run(rename("ours", "save")).is_err());
        assert!(world.run(rename("ghost", "x")).is_err());
        world.on_limit = "ours".to_string();
        assert!(
            world
                .run(rename("ours", "yours"))
                .unwrap_err()
                .contains("on_limit")
        );
    }

    #[test]
    fn a_profile_still_written_as_a_preset_is_renamed_and_deleted_too() {
        let mut world = World::new();
        world.text = "[presets.old.roles]\ndefault = [\"acme/old\"]\n".parse().unwrap();
        world.profiles.push(saved("old", "acme/old"));
        world.run(rename("old", "new")).unwrap();
        let (profiles, presets) = world.loaded();
        assert!(presets.is_empty());
        assert_eq!(profiles.entries["new"].roles["default"], chain(&["acme/old"]));
        world.run(delete("new")).unwrap();
        assert!(world.profiles_config().entries.is_empty());

        world.text = "[presets.old.roles]\ndefault = [\"acme/old\"]\n".parse().unwrap();
        world.profiles.push(saved("old", "acme/old"));
        world.run(delete("old")).unwrap();
        assert!(world.loaded().1.is_empty());
    }

    #[test]
    fn deleting_removes_a_saved_profile_and_refuses_what_must_stay() {
        let mut world = World::new();
        world.run(delete("mine")).unwrap();
        assert!(world.profiles_config().entries.is_empty());
        assert!(!world.names().contains(&"mine".to_string()));
        assert!(world.run(delete("claude")).unwrap_err().contains("built in"));
        assert!(world.run(delete("mine")).is_err());
        world.profiles.push(saved("limit", "acme/l"));
        world.on_limit = "limit".to_string();
        assert!(world.run(delete("limit")).unwrap_err().contains("on_limit"));
    }

    #[test]
    fn editing_a_role_changes_that_role_and_keeps_the_others() {
        let mut world = World::new();
        world
            .run(set("mine", "smol", Some(&["acme/tiny:low", "@default"])))
            .unwrap();
        let mine = &world.profiles_config().entries["mine"];
        assert_eq!(mine.roles["default"], chain(&["acme/mine"]));
        assert_eq!(mine.roles["smol"], chain(&["acme/tiny:low", "@default"]));
        world.run(set("mine", "smol", None)).unwrap();
        assert_eq!(world.profiles_config().entries["mine"].roles.len(), 1);
        assert!(
            world
                .run(set("mine", "default", None))
                .unwrap_err()
                .contains("no role left")
        );
        assert!(world.run(set("mine", "smol", Some(&["not a model"]))).is_err());
    }

    #[test]
    fn editing_a_built_in_writes_an_override_that_a_delete_takes_away() {
        let mut world = World::new();
        world.run(set("cheap", "smol", Some(&["acme/small"]))).unwrap();
        let builtin_roles = built_in("cheap").roles;
        let config = world.profiles_config();
        let over = &config.entries["cheap"].roles;
        assert_eq!(over["smol"], chain(&["acme/small"]));
        // The override holds every role of the built-in, as an entry replaces it whole.
        assert_eq!(over.len(), builtin_roles.len());
        let shown = world.profiles.iter().find(|p| p.name == "cheap").unwrap();
        assert_eq!(shown.source, ProfileSource::User);

        world.run(delete("cheap")).unwrap();
        assert!(!world.profiles_config().entries.contains_key("cheap"));
        let shown = world.profiles.iter().find(|p| p.name == "cheap").unwrap();
        assert_eq!(
            (shown.source, &shown.roles),
            (ProfileSource::Builtin, &builtin_roles)
        );
    }

    #[test]
    fn disabling_lists_a_built_in_in_the_config_and_enabling_takes_it_out() {
        let mut world = World::new();
        world.run(Edit::Disable("cheap".to_string())).unwrap();
        world.run(Edit::Disable("gpt".to_string())).unwrap();
        assert_eq!(world.profiles_config().disabled, ["cheap", "gpt"]);
        assert!(
            world
                .run(Edit::Disable("gpt".to_string()))
                .unwrap_err()
                .contains("already")
        );
        assert!(
            world
                .run(Edit::Disable("mine".to_string()))
                .unwrap_err()
                .contains("delete")
        );
        world.run(Edit::Enable("cheap".to_string())).unwrap();
        assert_eq!(world.profiles_config().disabled, ["gpt"]);
        world.run(Edit::Enable("gpt".to_string())).unwrap();
        assert!(!world.text.to_string().contains("disabled"));
        assert!(world.run(Edit::Enable("gpt".to_string())).is_err());
        world.on_limit = "claude".to_string();
        assert!(
            world
                .run(Edit::Disable("claude".to_string()))
                .unwrap_err()
                .contains("on_limit")
        );
    }

    #[test]
    fn a_project_profile_is_not_changed_from_here() {
        let mut world = World::new();
        world.profiles.push(Profile {
            source: ProfileSource::Project,
            ..saved("team", "acme/team")
        });
        for edit in [
            delete("team"),
            set("team", "smol", Some(&["acme/x"])),
            rename("team", "other"),
        ] {
            assert!(world.run(edit).unwrap_err().contains("project"));
        }
    }
}
