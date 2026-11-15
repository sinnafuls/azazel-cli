//! Ranks definitions by how the code refers to them and writes the best ones that fit a budget.

use std::collections::{HashMap, HashSet};

use crate::index::FileTags;

const DAMPING: f64 = 0.85;
const ITERATIONS: usize = 40;
/// A name defined in more files than this (`new`, `main`, `fmt`) says little about any of them.
const COMMON_NAME_FILES: usize = 5;
/// What a mention in the focus does to the weight of the references to it.
const FOCUS_SYMBOL_BOOST: f64 = 10.0;
/// What `⋮` and its line break cost in bytes.
const GAP: usize = "⋮\n".len();

/// Where the user's attention is: files (or directories) and the names of definitions.
#[derive(Debug, Default, Clone)]
pub struct Focus {
    pub items: Vec<String>,
}

impl Focus {
    fn matches_file(&self, path: &str) -> bool {
        self.items.iter().any(|item| {
            let item = item.trim_end_matches('/');
            !item.is_empty()
                && (path == item
                    || path.starts_with(&format!("{item}/"))
                    || path.ends_with(&format!("/{item}")))
        })
    }

    fn names(&self) -> HashSet<&str> {
        self.items.iter().map(String::as_str).collect()
    }
}

/// A type name, or a name long enough to belong to one thing; `len` and `env` are used
/// everywhere for the standard library's methods and say nothing about the one defined here.
fn distinctive(name: &str) -> bool {
    name.len() >= 8 || name.starts_with(|c: char| c.is_uppercase())
}

struct Edge {
    to: usize,
    name: usize,
    weight: f64,
}

/// The rank of every file and of every definition (`scores[file][def]`).
struct Ranks {
    files: Vec<f64>,
    scores: Vec<Vec<f64>>,
}

fn rank(files: &[FileTags], focus: &Focus) -> Ranks {
    let count = files.len();
    let mut defined: HashMap<&str, Vec<usize>> = HashMap::new();
    for (index, file) in files.iter().enumerate() {
        for def in file.tags.defs.iter().filter(|d| !d.name.is_empty()) {
            let owners = defined.entry(def.name.as_str()).or_default();
            if owners.last() != Some(&index) {
                owners.push(index);
            }
        }
    }
    let names = focus.names();
    let mut interned: HashMap<&str, usize> = HashMap::new();
    let mut edges: Vec<Vec<Edge>> = (0..count).map(|_| Vec::new()).collect();
    for (from, file) in files.iter().enumerate() {
        for (name, uses) in &file.tags.refs {
            let Some(owners) = defined.get(name.as_str()) else {
                continue;
            };
            let mut weight = f64::from(*uses).sqrt();
            if owners.len() > COMMON_NAME_FILES || name.starts_with('_') || !distinctive(name) {
                weight *= 0.1;
            }
            if names.contains(name.as_str()) {
                weight *= FOCUS_SYMBOL_BOOST;
            }
            let next = interned.len();
            let id = *interned.entry(name.as_str()).or_insert(next);
            for &to in owners.iter().filter(|&&to| to != from) {
                edges[from].push(Edge { to, name: id, weight });
            }
        }
    }
    let out: Vec<f64> = edges.iter().map(|e| e.iter().map(|e| e.weight).sum()).collect();

    let mut personal = vec![0.0; count];
    for (index, file) in files.iter().enumerate() {
        let defines_focus = file.tags.defs.iter().any(|d| names.contains(d.name.as_str()));
        if focus.matches_file(&file.path) || defines_focus {
            personal[index] = 1.0;
        }
    }
    let total: f64 = personal.iter().sum();
    if total == 0.0 {
        personal.iter_mut().for_each(|p| *p = 1.0 / count as f64);
    } else {
        personal.iter_mut().for_each(|p| *p /= total);
    }

    let mut rank = personal.clone();
    for _ in 0..ITERATIONS {
        let mut next = vec![0.0; count];
        let mut dangling = 0.0;
        for from in 0..count {
            if out[from] == 0.0 {
                dangling += rank[from];
                continue;
            }
            for edge in &edges[from] {
                next[edge.to] += rank[from] * edge.weight / out[from];
            }
        }
        for (index, value) in next.iter_mut().enumerate() {
            *value = (1.0 - DAMPING) * personal[index] + DAMPING * (*value + dangling * personal[index]);
        }
        rank = next;
    }

    // A definition earns what the files that use its name pass on to its file.
    let mut flow: Vec<HashMap<usize, f64>> = (0..count).map(|_| HashMap::new()).collect();
    for (from, list) in edges.iter().enumerate() {
        for edge in list {
            *flow[edge.to].entry(edge.name).or_default() += rank[from] * edge.weight / out[from];
        }
    }
    let scores = files
        .iter()
        .enumerate()
        .map(|(index, file)| {
            file.tags
                .defs
                .iter()
                .map(|def| {
                    if def.name.is_empty() {
                        return 0.0;
                    }
                    let earned = interned
                        .get(def.name.as_str())
                        .and_then(|id| flow[index].get(id))
                        .copied()
                        .unwrap_or(0.0);
                    if distinctive(&def.name) {
                        earned
                    } else {
                        earned * 0.1
                    }
                })
                .collect()
        })
        .collect();
    Ranks { files: rank, scores }
}

fn line(file: &FileTags, index: usize) -> String {
    let def = &file.tags.defs[index];
    format!("{}{}", "  ".repeat(def.depth as usize + 1), def.sig)
}

/// The outline of the most referenced definitions that fits `tokens` (four bytes to a token, as
/// everywhere in azazel): each file as `path:` and its signatures indented, with `⋮` where lines
/// were left out. Empty when there is nothing to show.
pub fn outline(files: &[FileTags], focus: &Focus, tokens: usize) -> String {
    if files.is_empty() || tokens == 0 {
        return String::new();
    }
    let ranks = rank(files, focus);
    let mut order: Vec<(usize, usize)> = files
        .iter()
        .enumerate()
        .flat_map(|(f, file)| {
            (0..file.tags.defs.len())
                .filter(|&d| !file.tags.defs[d].name.is_empty())
                .map(move |d| (f, d))
        })
        .collect();
    order.sort_by(|&(fa, da), &(fb, db)| {
        ranks.scores[fb][db]
            .total_cmp(&ranks.scores[fa][da])
            .then(ranks.files[fb].total_cmp(&ranks.files[fa]))
            .then(files[fa].path.cmp(&files[fb].path))
            .then(da.cmp(&db))
    });

    // A `⋮` is charged to every line, so the result cannot be over budget however the lines fall.
    let limit = tokens * 4;
    let mut used = 0;
    let mut kept: Vec<HashSet<usize>> = vec![HashSet::new(); files.len()];
    for (f, d) in order {
        let file = &files[f];
        let mut missing = Vec::new();
        let mut at = Some(d);
        while let Some(i) = at {
            if kept[f].contains(&i) {
                break;
            }
            missing.push(i);
            at = file.tags.defs[i].parent.map(|p| p as usize);
        }
        let mut cost: usize = missing.iter().map(|&i| line(file, i).len() + 1 + GAP).sum();
        if kept[f].is_empty() {
            cost += file.path.len() + 2;
        }
        if used + cost > limit {
            continue;
        }
        used += cost;
        kept[f].extend(missing);
    }

    let mut shown: Vec<usize> = (0..files.len()).filter(|&f| !kept[f].is_empty()).collect();
    shown.sort_by(|&a, &b| {
        ranks.files[b]
            .total_cmp(&ranks.files[a])
            .then(files[a].path.cmp(&files[b].path))
    });
    let mut text = String::new();
    for f in shown {
        let file = &files[f];
        text.push_str(&format!("{}:\n", file.path));
        let mut lines: Vec<usize> = kept[f].iter().copied().collect();
        lines.sort_unstable();
        let mut end: Option<u32> = None;
        for i in lines {
            let def = &file.tags.defs[i];
            if end.is_some_and(|end| def.line > end) {
                text.push_str("⋮\n");
            }
            end = Some(def.line + def.rows);
            text.push_str(&line(file, i));
            text.push('\n');
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::{Lang, extract};

    fn file(path: &str, source: &str) -> FileTags {
        FileTags {
            path: path.to_string(),
            tags: extract(Lang::Rust, source).unwrap(),
        }
    }

    fn project() -> Vec<FileTags> {
        vec![
            file("core.rs", "pub fn shared() {}\npub fn lonely() {}\n"),
            file("a.rs", "pub fn a_one() { shared(); }\n"),
            file("b.rs", "pub fn b_one() { shared(); }\n"),
            file("c.rs", "pub fn c_one() { shared(); a_one(); }\n"),
        ]
    }

    fn first_def(text: &str) -> &str {
        text.lines().find(|l| l.starts_with(' ')).unwrap().trim()
    }

    #[test]
    fn the_most_referenced_definition_comes_first() {
        let text = outline(&project(), &Focus::default(), 1000);
        assert_eq!(first_def(&text), "pub fn shared()");
        assert!(text.find("shared()").unwrap() < text.find("lonely").unwrap());
    }

    #[test]
    fn focus_changes_the_order() {
        let plain = outline(&project(), &Focus::default(), 1000);
        let focused = outline(
            &project(),
            &Focus {
                items: vec!["c.rs".to_string()],
            },
            1000,
        );
        assert!(focused.find("c.rs:").unwrap() < focused.find("b.rs:").unwrap());
        assert_ne!(plain, focused);
    }

    #[test]
    fn a_focus_symbol_pulls_its_definition_up() {
        let focused = outline(
            &project(),
            &Focus {
                items: vec!["a_one".to_string()],
            },
            1000,
        );
        assert_eq!(first_def(&focused), "pub fn a_one()");
    }

    #[test]
    fn the_budget_is_respected_and_the_best_survive() {
        let full = outline(&project(), &Focus::default(), 1000);
        let small = outline(&project(), &Focus::default(), 12);
        assert!(!small.is_empty());
        assert!(small.len() < full.len());
        assert!(small.len() <= 12 * 4, "{small}");
        assert!(small.contains("shared()"));
        assert!(!small.contains("lonely"));
    }

    #[test]
    fn skipped_lines_are_marked() {
        let files = vec![
            file(
                "a.rs",
                "fn first() { used(); }\n\nfn second() {}\n\nfn used() {}\n",
            ),
            file("b.rs", "fn other() { used(); }\n"),
        ];
        let text = outline(&files, &Focus::default(), 1000);
        assert!(text.contains("fn used()"));
        assert!(text.contains("⋮\n"), "{text}");
    }
}
