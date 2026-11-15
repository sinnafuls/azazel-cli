//! Word completion for the input: the rest of the word being typed, guessed from words the user
//! has written before. Pure text in, text out; where the words come from is the caller's business.
//!
//! [`query`] is the gate: it says whether the cursor is in plain prose worth completing and what
//! the word typed so far is. [`WordModel::suggest`] ranks the words that continue it by how often
//! they were seen and by the word before.

use std::collections::{BTreeMap, HashMap};

/// Characters that mark a token as code, a path, a mention or a command rather than prose.
const CODE_CHARS: &str = "/\\@_=:{}[]<>`$";
/// Shortest word the cursor must be in before anything is offered.
pub const MIN_PREFIX: usize = 3;
/// Words are kept and offered up to this many letters.
const MAX_WORD: usize = 24;
/// Longest input, in bytes, and longest line that still get completion.
const MAX_BUFFER: usize = 20_000;
const MAX_LINE: usize = 1_000;
/// Most distinct words and word pairs the model keeps; past it only known ones are counted.
const MAX_WORDS: usize = 60_000;
const MAX_PAIRS: usize = 200_000;
/// What one sighting of the word before is worth against one sighting of the word itself.
const PAIR_WEIGHT: u32 = 20;

/// The word being typed and the one before it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    /// The letters typed so far, as typed.
    pub prefix: String,
    /// The word before it, lower case, when there is one.
    pub previous: Option<String>,
}

fn is_word_char(c: char) -> bool {
    c.is_alphabetic() || c == '\''
}

/// Whether the token reads as a plain word, possibly with a capital first letter.
fn is_prose(token: &str) -> bool {
    if token.chars().any(|c| CODE_CHARS.contains(c) || c.is_numeric()) {
        return false;
    }
    // Private-use characters stand for atoms (file and image chips) in the editor's text.
    if token
        .chars()
        .any(|c| ('\u{E000}'..='\u{F8FF}').contains(&c) || c as u32 >= 0xF_0000)
    {
        return false;
    }
    // camelCase, and ALL CAPS, are names; a capital opens a word at most.
    !token.chars().skip(1).any(char::is_uppercase)
}

/// Whether completion applies where the cursor is, and if it does, the word to complete.
///
/// It does not apply to slash commands, shell lines, `@` mentions, paths, code-looking tokens,
/// fenced code, a cursor that is not at the end of its line, a word of fewer than
/// [`MIN_PREFIX`] letters, or very long input.
pub fn query(text: &str, cursor: usize) -> Option<Query> {
    if cursor > text.len() || !text.is_char_boundary(cursor) || text.len() > MAX_BUFFER {
        return None;
    }
    if text[cursor..].chars().next().is_some_and(|c| c != '\n') {
        return None;
    }
    let lead = text.trim_start();
    if lead.starts_with(['/', '!']) || lead.starts_with("->") || lead.starts_with("=>") {
        return None;
    }
    let before = &text[..cursor];
    if before.matches("```").count() % 2 == 1 {
        return None;
    }
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let line = &before[line_start..];
    if line.len() > MAX_LINE {
        return None;
    }
    let token_start = line
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map_or(0, |(i, c)| i + c.len_utf8());
    let token = &line[token_start..];
    if !is_prose(token) {
        return None;
    }
    let prefix_start = token
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_word_char(*c))
        .last()
        .map(|(i, _)| i)?;
    let prefix = &token[prefix_start..];
    if prefix.chars().count() < MIN_PREFIX {
        return None;
    }
    let previous = before[..line_start + token_start]
        .split_whitespace()
        .next_back()
        .map(|word| word.trim_matches(|c: char| !c.is_alphabetic()).to_lowercase())
        .filter(|word| !word.is_empty());
    Some(Query {
        prefix: prefix.to_string(),
        previous,
    })
}

/// The prose words of `text`, lower case, each with the word before it.
fn words_of(text: &str) -> impl Iterator<Item = (String, Option<String>)> + '_ {
    let mut previous: Option<String> = None;
    text.split_whitespace().filter_map(move |token| {
        let core = token.trim_matches(|c: char| !c.is_alphabetic());
        let usable = !core.is_empty()
            && core.chars().all(is_word_char)
            && is_prose(core)
            && (3..=MAX_WORD).contains(&core.chars().count());
        if !usable {
            previous = None;
            return None;
        }
        let word = core.to_lowercase();
        Some((word.clone(), previous.replace(word)))
    })
}

/// What was written before, as counts of words and of word pairs.
#[derive(Debug, Default, Clone)]
pub struct WordModel {
    words: BTreeMap<String, u32>,
    pairs: HashMap<String, u32>,
}

impl WordModel {
    pub fn new() -> WordModel {
        WordModel::default()
    }

    /// How many distinct words are known.
    pub fn len(&self) -> usize {
        self.words.len()
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    fn count(&mut self, word: String, previous: Option<String>, weight: u32) {
        if let Some(previous) = previous
            && (self.pairs.len() < MAX_PAIRS || self.pairs.contains_key(&format!("{previous} {word}")))
        {
            *self.pairs.entry(format!("{previous} {word}")).or_default() += weight;
        }
        if self.words.len() < MAX_WORDS || self.words.contains_key(&word) {
            *self.words.entry(word).or_default() += weight;
        }
    }

    /// Counts the prose words of `text`, each `weight` times.
    pub fn learn(&mut self, text: &str, weight: u32) {
        for (word, previous) in words_of(text) {
            self.count(word, previous, weight);
        }
    }

    /// Counts the words a file path is made of (`src/ReadGuard.rs` gives `read`, `guard`).
    pub fn learn_path(&mut self, path: &str) {
        let mut part = String::new();
        let mut last_lower = false;
        let flush = |part: &mut String, model: &mut WordModel| {
            if (4..=MAX_WORD).contains(&part.chars().count()) {
                model.count(std::mem::take(part), None, 1);
            }
            part.clear();
        };
        for c in path.chars() {
            if !c.is_alphabetic() {
                flush(&mut part, self);
                last_lower = false;
            } else {
                if c.is_uppercase() && last_lower {
                    flush(&mut part, self);
                }
                last_lower = c.is_lowercase();
                part.extend(c.to_lowercase());
            }
        }
        flush(&mut part, self);
    }

    /// Adds everything `other` knows.
    pub fn merge(&mut self, other: WordModel) {
        for (word, n) in other.words {
            if self.words.len() < MAX_WORDS || self.words.contains_key(&word) {
                *self.words.entry(word).or_default() += n;
            }
        }
        for (pair, n) in other.pairs {
            if self.pairs.len() < MAX_PAIRS || self.pairs.contains_key(&pair) {
                *self.pairs.entry(pair).or_default() += n;
            }
        }
    }

    /// The letters that finish the word before the cursor, when the gate lets completion in and
    /// a known word continues it.
    pub fn suggest(&self, text: &str, cursor: usize) -> Option<String> {
        let query = query(text, cursor)?;
        let typed = query.prefix.to_lowercase();
        let mut best: Option<(&str, u32)> = None;
        for (word, count) in self
            .words
            .range(typed.clone()..)
            .take_while(|(word, _)| word.starts_with(&typed))
            .filter(|(word, _)| word.len() > typed.len())
        {
            let pair = query
                .previous
                .as_ref()
                .and_then(|previous| self.pairs.get(&format!("{previous} {word}")))
                .copied()
                .unwrap_or(0);
            let score = count + PAIR_WEIGHT * pair;
            // Words come in alphabetical order: on a tie the shorter one, then the earlier one.
            let better = best.is_none_or(|(held, top)| {
                score > top || (score == top && word.chars().count() < held.chars().count())
            });
            if better {
                best = Some((word, score));
            }
        }
        let (word, score) = best?;
        // Typing a whole word that is at least as common as anything that continues it is done.
        if self.words.get(&typed).is_some_and(|known| *known >= score) {
            return None;
        }
        Some(word.chars().skip(typed.chars().count()).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(texts: &[&str]) -> WordModel {
        let mut model = WordModel::new();
        for text in texts {
            model.learn(text, 1);
        }
        model
    }

    fn suggest(model: &WordModel, typed: &str) -> Option<String> {
        model.suggest(typed, typed.len())
    }

    #[test]
    fn the_most_frequent_continuation_wins_and_the_word_before_breaks_the_tie() {
        let history = model(&[
            "refactor the parser",
            "refactor the parser again",
            "please review it",
            "run the review",
        ]);
        assert_eq!(suggest(&history, "refa").as_deref(), Some("ctor"));
        assert_eq!(suggest(&history, "ref").as_deref(), Some("actor"));
        assert_eq!(suggest(&history, "the par").as_deref(), Some("ser"));

        // `rev` continues as `review` only; `pa` is too short, as is anything under three letters.
        assert_eq!(suggest(&history, "rev").as_deref(), Some("iew"));
        assert_eq!(suggest(&history, "pa"), None);

        // Same count for two words: the word before decides, else the shorter word.
        let tied = model(&["stable build", "staging area", "the staging area"]);
        assert_eq!(suggest(&tied, "the sta").as_deref(), Some("ging"));
        assert_eq!(suggest(&tied, "a sta").as_deref(), Some("ging"));
        let tied = model(&["stable build", "staging area"]);
        assert_eq!(suggest(&tied, "a sta").as_deref(), Some("ble"));
    }

    #[test]
    fn a_word_typed_in_full_is_left_alone_unless_a_longer_one_is_commoner() {
        let history = model(&["the the the them"]);
        assert_eq!(suggest(&history, "the"), None);
        let history = model(&["the them them them"]);
        assert_eq!(suggest(&history, "the").as_deref(), Some("m"));
        let history = model(&["plan plan plan planning planning"]);
        assert_eq!(suggest(&history, "plan"), None);
        assert_eq!(suggest(&history, "planni").as_deref(), Some("ng"));
    }

    #[test]
    fn the_gate_keeps_completion_to_plain_prose_at_the_end_of_a_line() {
        let history = model(&["authentication middleware configuration"]);
        for blocked in [
            "/authen",
            "  /authen",
            "!authen",
            "look at @authen",
            "src/authen",
            "C:\\authen",
            "my_authen",
            "x=authen",
            "name:authen",
            "{authen",
            "`authen",
            "Authen_x",
            "authenTication",
            "AUTHEN",
            "auth3n",
            "authen\u{10_0000}",
            "```\nauthen",
            "au",
            "authen ",
        ] {
            assert_eq!(suggest(&history, blocked), None, "{blocked:?}");
        }
        for allowed in [
            "authen",
            "check the authen",
            "first line\nsecond authen",
            "Authen",
            "(authen",
        ] {
            assert!(suggest(&history, allowed).is_some(), "{allowed:?}");
        }
        assert_eq!(suggest(&history, "Authen").as_deref(), Some("tication"));

        // Mid-word, or before more text on the line: nothing.
        assert_eq!(history.suggest("authen here", 5), None);
        assert_eq!(
            history.suggest("authen", 5),
            None.or(history.suggest("authen", 5))
        );
        // The cursor at the end of a line that is not the last one.
        assert_eq!(history.suggest("authen\nmore", 6).as_deref(), Some("tication"));
        // Very long input.
        let long = format!("{} authen", "word ".repeat(5_000));
        assert_eq!(suggest(&history, &long), None);
    }

    #[test]
    fn code_and_names_are_not_learned() {
        let mut learned = model(&["call parseConfig on src/main.rs with HTTP_PROXY=1 and $HOME"]);
        learned.learn("and use the parser", 1);
        assert_eq!(suggest(&learned, "pars").as_deref(), Some("er"));
        assert_eq!(suggest(&learned, "mai"), None);
        assert_eq!(suggest(&learned, "prox"), None);
    }

    #[test]
    fn file_names_give_their_words() {
        let mut files = WordModel::new();
        files.learn_path("crates/azazel-core/src/ReadGuard.rs");
        assert_eq!(suggest(&files, "gua").as_deref(), Some("rd"));
        assert_eq!(suggest(&files, "rea").as_deref(), Some("d"));
        let mut other = WordModel::new();
        other.learn("guarantee guarantee", 1);
        files.merge(other);
        assert_eq!(suggest(&files, "gua").as_deref(), Some("rantee"));
    }
}
