//! Ranking tools against a query with BM25 over their names and descriptions.

use std::collections::HashMap;

const K1: f64 = 1.2;
const B: f64 = 0.75;
/// A word in the name counts as this many occurrences.
const NAME_WEIGHT: usize = 3;

/// Lower-case words; `camelCase`, `snake_case` and `kebab-case` names split into their parts.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut word = String::new();
    let mut previous_lower = false;
    for c in text.chars() {
        if !c.is_alphanumeric() {
            if !word.is_empty() {
                tokens.push(std::mem::take(&mut word));
            }
            previous_lower = false;
            continue;
        }
        if c.is_uppercase() && previous_lower && !word.is_empty() {
            tokens.push(std::mem::take(&mut word));
        }
        previous_lower = c.is_lowercase() || c.is_numeric();
        word.extend(c.to_lowercase());
    }
    if !word.is_empty() {
        tokens.push(word);
    }
    tokens
}

/// Indices into `tools` (`(name, description)` pairs) of the entries matching `query`, best first,
/// at most `limit`. Entries sharing no word with the query are left out; ties go to the earlier
/// entry.
pub fn rank(tools: &[(String, String)], query: &str, limit: usize) -> Vec<usize> {
    let documents: Vec<Vec<String>> = tools
        .iter()
        .map(|(name, description)| {
            let name_words = tokenize(name);
            let mut words = tokenize(description);
            for _ in 0..NAME_WEIGHT {
                words.extend(name_words.iter().cloned());
            }
            words
        })
        .collect();
    let total: usize = documents.iter().map(Vec::len).sum();
    if total == 0 {
        return Vec::new();
    }
    let average = total as f64 / documents.len() as f64;

    let mut terms = tokenize(query);
    terms.sort();
    terms.dedup();
    let mut containing: HashMap<&str, usize> = HashMap::new();
    for term in &terms {
        let count = documents.iter().filter(|words| words.contains(term)).count();
        containing.insert(term, count);
    }

    let mut scored: Vec<(usize, f64)> = Vec::new();
    for (index, words) in documents.iter().enumerate() {
        let mut score = 0.0;
        for term in &terms {
            let frequency = words.iter().filter(|word| *word == term).count() as f64;
            if frequency == 0.0 {
                continue;
            }
            let n = containing[term.as_str()] as f64;
            let idf = ((documents.len() as f64 - n + 0.5) / (n + 0.5) + 1.0).ln();
            let length = words.len() as f64 / average;
            score += idf * frequency * (K1 + 1.0) / (frequency + K1 * (1.0 - B + B * length));
        }
        if score > 0.0 {
            scored.push((index, score));
        }
    }
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    scored.into_iter().take(limit).map(|(index, _)| index).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools() -> Vec<(String, String)> {
        [
            (
                "mcp__git__commitChanges",
                "Record staged changes in the repository",
            ),
            ("mcp__echo__echo", "Returns the text it was given"),
            (
                "mcp__db__run_query",
                "Run a SQL query against the database and return rows as text",
            ),
            ("mcp__fs__read-file", "Read a file from disk"),
        ]
        .into_iter()
        .map(|(name, description)| (name.to_string(), description.to_string()))
        .collect()
    }

    #[test]
    fn names_split_into_words() {
        assert_eq!(
            tokenize("mcp__git__commitChanges"),
            ["mcp", "git", "commit", "changes"]
        );
        assert_eq!(
            tokenize("read-file v2 HTTPServer"),
            ["read", "file", "v2", "httpserver"]
        );
        assert!(tokenize(" _-_ ").is_empty());
    }

    #[test]
    fn the_best_match_comes_first_and_non_matches_are_left_out() {
        let tools = tools();
        assert_eq!(rank(&tools, "echo", 8), [1]);
        assert_eq!(rank(&tools, "commit my changes", 8), [0]);
        // "text" is in two descriptions; the query's rarer word decides.
        assert_eq!(rank(&tools, "sql text", 8), [2, 1]);
        assert_eq!(rank(&tools, "read a FILE", 1), [3]);
        assert!(rank(&tools, "kubernetes", 8).is_empty());
        assert!(rank(&tools, "", 8).is_empty());
        assert!(rank(&[], "echo", 8).is_empty());
    }

    #[test]
    fn a_word_in_the_name_outweighs_the_same_word_in_a_description() {
        let tools = vec![
            ("mcp__a__describe".to_string(), "Shows the query plan".to_string()),
            ("mcp__b__query".to_string(), "Runs it".to_string()),
        ];
        assert_eq!(rank(&tools, "query", 8), [1, 0]);
    }
}
