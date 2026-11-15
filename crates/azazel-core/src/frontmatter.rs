//! YAML frontmatter of Markdown files (skills, commands, agent definitions).

use serde_json::{Map, Value};

/// Splits `---\n<yaml>\n---\n<body>`. `None` when the text does not start with a frontmatter block.
pub fn split(text: &str) -> Option<(&str, &str)> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let rest = text.strip_prefix("---")?;
    let rest = rest.strip_prefix("\r\n").or_else(|| rest.strip_prefix('\n'))?;
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            return Some((&rest[..offset], &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    None
}

/// Parses frontmatter YAML strictly: it must be a mapping with string keys, and a key may appear
/// only once.
pub fn parse(yaml: &str) -> Result<Map<String, Value>, String> {
    let value: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(yaml).map_err(|e| format!("invalid frontmatter: {e}"))?;
    match to_json(value)? {
        Value::Object(map) => Ok(map),
        Value::Null => Ok(Map::new()),
        _ => Err("invalid frontmatter: expected `key: value` pairs".to_string()),
    }
}

fn to_json(value: serde_yaml_ng::Value) -> Result<Value, String> {
    use serde_yaml_ng::Value as Y;
    Ok(match value {
        Y::Null => Value::Null,
        Y::Bool(b) => Value::Bool(b),
        Y::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
            (Some(i), _, _) => Value::from(i),
            (_, Some(u), _) => Value::from(u),
            (_, _, Some(f)) => serde_json::Number::from_f64(f)
                .map(Value::Number)
                .unwrap_or(Value::Null),
            _ => Value::Null,
        },
        Y::String(s) => Value::String(s),
        Y::Sequence(items) => Value::Array(items.into_iter().map(to_json).collect::<Result<_, _>>()?),
        Y::Mapping(map) => {
            let mut out = Map::new();
            for (key, value) in map {
                let Y::String(key) = key else {
                    return Err("invalid frontmatter: keys must be strings".to_string());
                };
                out.insert(key, to_json(value)?);
            }
            Value::Object(out)
        }
        Y::Tagged(tagged) => to_json(tagged.value)?,
    })
}

/// A string field, or a list of strings; a plain string may be comma-separated.
pub fn string_list(value: Option<&Value>) -> Option<Vec<String>> {
    match value? {
        Value::String(s) => Some(
            s.split(',')
                .map(|part| part.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect(),
        ),
        Value::Array(items) => Some(
            items
                .iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .collect(),
        ),
        _ => None,
    }
}

/// Like [`string_list`], but a plain string is one entry even when it contains commas: for values
/// such as tool rules whose patterns may themselves contain commas.
pub fn string_or_list(value: Option<&Value>) -> Option<Vec<String>> {
    match value? {
        Value::String(s) => Some(split_top_level(s)),
        other => string_list(Some(other)),
    }
}

/// Splits on commas that are not inside parentheses: `read, shell(git diff:*), grep`.
fn split_top_level(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();
    for c in text.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    parts.push(current);
    parts
        .into_iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parse_converts_yaml_and_rejects_duplicates_and_non_mappings() {
        let map =
            parse("name: x\nflag: true\nn: 3\nlist: [a, b]\nnested:\n  k: v\nfolded: >-\n  two\n  lines\n")
                .unwrap();
        assert_eq!(
            Value::Object(map),
            json!({"name": "x", "flag": true, "n": 3, "list": ["a", "b"], "nested": {"k": "v"}, "folded": "two lines"})
        );
        assert!(parse("").unwrap().is_empty());
        assert!(
            parse("name: a\nname: b\n")
                .unwrap_err()
                .contains("invalid frontmatter")
        );
        assert!(parse("- a\n- b\n").is_err());
        // The classic silent failure: an unquoted colon inside a value.
        assert!(parse("description: Use when: always\n").is_err());
    }

    #[test]
    fn lists_accept_strings_and_arrays() {
        assert_eq!(
            string_list(Some(&json!("read, grep ,glob"))),
            Some(vec!["read".into(), "grep".into(), "glob".into()])
        );
        assert_eq!(
            string_list(Some(&json!(["a", " b "]))),
            Some(vec!["a".into(), "b".into()])
        );
        assert_eq!(string_list(None), None);
        assert_eq!(
            string_or_list(Some(&json!(
                "read, shell(git diff:*), shell(awk '{print $1, $2}'), grep"
            ))),
            Some(vec![
                "read".into(),
                "shell(git diff:*)".into(),
                "shell(awk '{print $1, $2}')".into(),
                "grep".into()
            ])
        );
    }
}
