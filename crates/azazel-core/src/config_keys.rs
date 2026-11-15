//! Every key of the configuration as a dotted path (`retry.max_attempts`), so that the docs can be
//! compared with the code (`cargo xtask docs-check`).
//!
//! A table with fixed keys is walked through its default value. A table whose entries the user
//! names has no entry by default; it is listed in [`named_entries`] with the type of one entry,
//! and its keys are the fields that type deserialises, with `*` for the name
//! (`providers.*.base_url`).

use std::collections::BTreeMap;

use azazel_ai::catalog::ModelOverride;
use azazel_ai::providers::presets::ProviderSettings;
use serde::Deserialize;
use serde::de::value::Error as NoValue;
use serde_json::Value;

use crate::config::{
    AgentOverride, Config, DebugAdapterConfig, EmbeddingsConfig, HookConfig, LspServerConfig, PresetConfig,
    StreamRuleConfig,
};

/// Asks a type for the fields it deserialises without giving it a value: `serde` passes the
/// names to `deserialize_struct`, which keeps them and stops.
struct Fields<'a>(&'a mut Vec<&'static str>);

impl<'de> serde::Deserializer<'de> for Fields<'_> {
    type Error = NoValue;

    fn deserialize_any<V: serde::de::Visitor<'de>>(self, _: V) -> Result<V::Value, NoValue> {
        Err(serde::de::Error::custom("not a struct"))
    }

    fn deserialize_struct<V: serde::de::Visitor<'de>>(
        self,
        _: &'static str,
        fields: &'static [&'static str],
        _: V,
    ) -> Result<V::Value, NoValue> {
        self.0.extend_from_slice(fields);
        Err(serde::de::Error::custom("fields read"))
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf option
        unit unit_struct newtype_struct seq tuple tuple_struct map enum identifier ignored_any
    }
}

/// The names a struct's fields have in a configuration file.
fn fields_of<'de, T: Deserialize<'de>>() -> Vec<&'static str> {
    let mut fields = Vec::new();
    // Always an error: the fields are what is wanted, and no value is given.
    let _ = T::deserialize(Fields(&mut fields));
    fields
}

/// The tables whose entries are named by the user, or absent by default, as
/// `(path of one entry, the fields of an entry)`.
fn named_entries() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![
        ("providers.*", fields_of::<ProviderSettings>()),
        ("models.*", fields_of::<ModelOverride>()),
        ("agents.*", fields_of::<AgentOverride>()),
        ("presets.*", fields_of::<PresetConfig>()),
        ("hooks.*", fields_of::<HookConfig>()),
        ("stream_rules", fields_of::<StreamRuleConfig>()),
        ("embeddings", fields_of::<EmbeddingsConfig>()),
        ("lsp.servers.*", fields_of::<LspServerConfig>()),
        ("debug.adapters.*", fields_of::<DebugAdapterConfig>()),
    ]
}

fn walk(
    path: &str,
    value: &Value,
    named: &[(&'static str, Vec<&'static str>)],
    out: &mut BTreeMap<String, Option<String>>,
) {
    if let Some((entry, fields)) = named
        .iter()
        .find(|(entry, _)| entry.strip_suffix(".*").unwrap_or(entry) == path)
    {
        out.extend(fields.iter().map(|field| (format!("{entry}.{field}"), None)));
        // Keys of its own beside the entries the user names (`agents.default`).
        for (key, member) in value.as_object().into_iter().flatten() {
            walk(&format!("{path}.{key}"), member, &[], out);
        }
        return;
    }
    match value {
        Value::Object(members) if !members.is_empty() => {
            for (key, member) in members {
                walk(&format!("{path}.{key}"), member, named, out);
            }
        }
        // JSON writes strings, numbers, booleans, lists and empty tables as TOML does.
        Value::Null => {
            out.insert(path.to_string(), None);
        }
        default => {
            out.insert(path.to_string(), Some(default.to_string()));
        }
    }
}

/// Every key a configuration file may set, sorted, with its default as a TOML literal (`3`,
/// `"manual"`, `[]`, `{}`) when it has one. A key that holds a free table or a list is one path
/// (`tools.format`, `permissions.allow`); the name of an entry is `*` (`lsp.servers.*.command`),
/// and the keys of such entries come without a default. `roles` lists the roles that exist
/// without configuration, and `default`, which the user must set.
pub fn keys() -> Vec<(String, Option<String>)> {
    let named = named_entries();
    let mut out = BTreeMap::new();
    out.insert("roles.default".to_string(), None);
    // JSON rather than TOML: a setting that is absent by default is `null` there, not left out.
    let defaults = serde_json::to_value(Config::default()).unwrap_or_default();
    for (table, value) in defaults.as_object().into_iter().flatten() {
        walk(table, value, &named, &mut out);
    }
    out.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_tables_are_walked_and_named_entries_list_their_fields() {
        let keys = keys();
        let has = |path: &str| keys.iter().any(|(p, _)| p == path);
        let default = |path: &str| {
            let key = keys.iter().find(|(p, _)| p == path);
            key.and_then(|(_, default)| default.as_deref())
        };
        assert_eq!(default("retry.max_attempts"), Some("3"));
        assert_eq!(default("permissions.mode"), Some("\"manual\""));
        assert_eq!(default("permissions.allow"), Some("[]"));
        assert_eq!(default("tools.format"), Some("{}"));
        assert_eq!(default("roles.smol"), Some("[\"@default\"]"));
        assert_eq!(default("roles.default"), None);
        assert_eq!(default("providers.*.base_url"), None);
        // A plain key, a nested table, a free table and a list.
        assert!(has("retry.max_attempts"));
        assert!(has("tools.scan.secrets"));
        assert!(has("tools.format"));
        assert!(has("permissions.allow"));
        assert!(has("statusline.segments"));
        // Roles with and without a built-in chain.
        assert!(has("roles.default"));
        assert!(has("roles.smol"));
        // Entries the user names, with the names the file uses (`if`, not the field `when`).
        assert!(has("providers.*.base_url"));
        assert!(has("models.*.context_window"));
        assert!(has("agents.*.thinking"));
        assert_eq!(default("agents.default"), Some("\"build\""));
        assert!(has("presets.*.roles"));
        assert!(has("hooks.*.if"));
        assert!(has("hooks.*.async"));
        assert!(!has("hooks.*.when"));
        assert!(has("stream_rules.pattern"));
        assert!(has("embeddings.provider"));
        assert!(has("lsp.servers.*.warm_files"));
        // The tables themselves are not keys.
        assert!(!has("providers"));
        assert!(!has("lsp.servers"));
        assert!(!has("embeddings"));
    }

    /// A table that is added with entries the user names, or absent by default, must be given
    /// its entry type in `named_entries`, or its keys would go unlisted.
    #[test]
    fn every_table_that_is_empty_by_default_is_accounted_for() {
        let defaults = serde_json::to_value(Config::default()).unwrap();
        let named = named_entries();
        // Tables whose keys are not fixed: role names, and nothing else.
        let free = ["roles"];
        for (table, value) in defaults.as_object().unwrap() {
            let empty = match value {
                Value::Null => true,
                Value::Object(members) => members.is_empty(),
                Value::Array(items) => items.is_empty(),
                _ => false,
            };
            let known = named
                .iter()
                .any(|(entry, _)| entry.split('.').next() == Some(table.as_str()));
            assert!(
                !empty || known || free.contains(&table.as_str()),
                "`{table}` has no keys by default: add its entry type to `named_entries`"
            );
        }
        assert_eq!(fields_of::<Config>().len(), defaults.as_object().unwrap().len());
    }
}
