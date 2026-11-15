//! The model rows of `/models`: which rows a search keeps and in which order, and how a row
//! and the detail under the list are drawn.

use azazel_ai::ModelSpec;
use azazel_protocol::{ModelCost, ModelRow};
use azazel_tui::{Glyph, Line, RESET, Role, Theme, text};
use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

/// Columns before the name: the rail of the selected row and a space.
pub(super) const RAIL: usize = 2;
/// Columns between two columns of a row.
const GAP: usize = 2;
/// The fewest columns the name of a model keeps before a column is dropped for it.
const MIN_NAME: usize = 16;

/// `provider/id` of a model written with or without a thinking level; a role stays as it is.
pub(super) fn bare(spec: &str) -> String {
    match spec.parse::<ModelSpec>() {
        Ok(ModelSpec::Model(model)) => format!("{}/{}", model.provider, model.model),
        _ => spec.to_string(),
    }
}

/// A model that costs nothing per token.
pub(super) fn is_free(row: &ModelRow) -> bool {
    row.cost
        .is_some_and(|cost| cost.input == 0.0 && cost.output == 0.0)
}

fn price_of(cost: &ModelCost) -> String {
    crate::models::price(&azazel_ai::Cost {
        input: cost.input,
        output: cost.output,
        cache_read: 0.0,
        cache_write: 0.0,
    })
}

/// What a model costs, the way the price column says it: `subscription` through a sign-in,
/// `free`, or the dollars per million tokens in and out; empty when that is not known.
pub(super) fn price(row: &ModelRow) -> String {
    match &row.cost {
        _ if row.login => "subscription".to_string(),
        Some(_) if is_free(row) => "free".to_string(),
        Some(cost) => price_of(cost),
        None => String::new(),
    }
}

/// How well a row matches a search: the tier (0 an exact name, 1 the text contains the
/// search, 2 a loose match) and the matcher's score within it.
pub(super) type Hit = (u8, u32);

/// Matches rows against a search.
pub(super) struct Ranker {
    matcher: Matcher,
}

impl Default for Ranker {
    fn default() -> Ranker {
        Ranker {
            matcher: Matcher::new(Config::DEFAULT),
        }
    }
}

impl Ranker {
    /// How each of `rows` matches `query`: on `provider/id`, the name, and the word `free` for
    /// a model that costs nothing. Case is ignored unless the search has a capital letter.
    pub(super) fn hits(&mut self, query: &str, rows: &[ModelRow]) -> Vec<Option<Hit>> {
        let query = query.trim();
        let pattern = Pattern::new(query, CaseMatching::Smart, Normalization::Smart, AtomKind::Fuzzy);
        let lower = query.to_lowercase();
        let mut buf = Vec::new();
        rows.iter()
            .map(|row| {
                let name = row.name.as_deref().unwrap_or_default();
                let free = if is_free(row) { " free" } else { "" };
                let hay = format!("{} {name}{free}", row.model);
                let score = pattern.score(Utf32Str::new(&hay, &mut buf), &mut self.matcher)?;
                let exact = [row.id.as_str(), row.model.as_str(), name]
                    .iter()
                    .any(|known| known.to_lowercase() == lower);
                let tier = match () {
                    _ if exact => 0,
                    _ if hay.to_lowercase().contains(&lower) => 1,
                    _ => 2,
                };
                Some((tier, score))
            })
            .collect()
    }
}

/// `candidates` (indices into the rows `hits` was made for) that matched, best first: by
/// tier, then by score; ties keep their order.
pub(super) fn ranked(hits: &[Option<Hit>], candidates: &[usize]) -> Vec<usize> {
    let mut kept: Vec<(usize, Hit)> = candidates
        .iter()
        .filter_map(|&index| hits.get(index).copied().flatten().map(|hit| (index, hit)))
        .collect();
    kept.sort_by(|(_, a), (_, b)| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    kept.into_iter().map(|(index, _)| index).collect()
}

/// The columns a list of rows shows beside the names, as wide as their widest cell; those that
/// do not fit are left out, the price first, then the output limit, then the context window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct Columns {
    pub context: usize,
    pub output: usize,
    pub price: usize,
}

impl Columns {
    pub(super) fn of<'a>(rows: impl Iterator<Item = &'a ModelRow>, width: usize) -> Columns {
        let mut columns = Columns::default();
        for row in rows {
            columns.context = columns
                .context
                .max(text::width(&crate::models::compact(row.context_window)));
            columns.output = columns
                .output
                .max(text::width(&crate::models::compact(row.max_output)));
            columns.price = columns.price.max(text::width(&price(row)));
        }
        // The marks of what a model can do: two cells.
        let fixed = RAIL + MIN_NAME + GAP + 2;
        let needed = |c: &Columns| {
            fixed
                + [c.context, c.output, c.price]
                    .iter()
                    .filter(|w| **w > 0)
                    .map(|w| w + GAP)
                    .sum::<usize>()
        };
        let drops: [fn(&mut Columns); 3] = [|c| c.price = 0, |c| c.output = 0, |c| c.context = 0];
        for drop in drops {
            if needed(&columns) <= width {
                break;
            }
            drop(&mut columns);
        }
        columns
    }

    /// Columns the cells take on the right of a row, gaps included.
    fn width(&self) -> usize {
        [self.context, self.output, self.price]
            .iter()
            .filter(|w| **w > 0)
            .map(|w| w + GAP)
            .sum::<usize>()
            + GAP
            + 2
    }
}

/// What a row says beside the name of its model.
pub(super) struct Marks<'a> {
    /// The model the session is on.
    pub current: bool,
    /// The roles that name the model first.
    pub tags: &'a [String],
    pub selected: bool,
    /// Its provider cannot be used.
    pub dim: bool,
}

/// One model: the rail, `provider/id`, the current mark and the role tags; then on the right
/// the context window, the output limit, the price and the marks of thinking and images.
pub(super) fn row(
    model: &ModelRow,
    marks: &Marks<'_>,
    columns: &Columns,
    width: usize,
    theme: &Theme,
) -> Line {
    let ellipsis = theme.glyph(Glyph::Ellipsis);
    let right_width = columns.width().min(width.saturating_sub(RAIL + 1));
    let name_room = width.saturating_sub(RAIL + right_width);
    let mut cells = String::new();
    for (shown, value) in [
        (columns.context, crate::models::compact(model.context_window)),
        (columns.output, crate::models::compact(model.max_output)),
        (columns.price, price(model)),
    ] {
        if shown > 0 {
            cells.push_str(&" ".repeat(GAP));
            cells.push_str(&format!("{value:>shown$}"));
        }
    }
    let mark = |on: bool, glyph: Glyph| if on { theme.glyph(glyph) } else { " " };
    cells.push_str(&" ".repeat(GAP));
    cells.push_str(mark(model.reasoning, Glyph::Reasons));
    cells.push_str(mark(model.vision, Glyph::Vision));
    let cells = text::truncate(&cells, right_width, "");

    // The name keeps what it needs; the tags take what is left.
    let name = text::truncate(&text::sanitize(&model.model), name_room, ellipsis);
    let mut used = text::width(&name);
    let mut after = String::new();
    if marks.current && used + 2 <= name_room {
        after.push(' ');
        after.push_str(&theme.paint(Role::Success, theme.glyph(Glyph::Bullet)));
        used += 2;
    }
    if !marks.tags.is_empty() && used + 2 < name_room {
        let tags = text::truncate(&marks.tags.join(" "), name_room - used - 1, ellipsis);
        used += 1 + text::width(&tags);
        after.push(' ');
        after.push_str(&theme.paint(Role::Accent, &tags));
    }
    let pad = " ".repeat(name_room.saturating_sub(used));
    let body = if marks.dim {
        format!(
            "{}{pad}{}",
            theme.paint(Role::Muted, &name),
            theme.paint(Role::Muted, &cells)
        )
    } else {
        format!("{name}{after}{pad}{}", theme.paint(Role::Muted, &cells))
    };
    rail_row(&body, marks.selected, width, theme)
}

/// A row of a list with the rail column: the selected one on the selection band with its rail
/// in the accent and its text in bold.
pub(super) fn rail_row(body: &str, selected: bool, width: usize, theme: &Theme) -> Line {
    if !selected {
        return text::truncate(&format!("{}{body}", " ".repeat(RAIL)), width, "");
    }
    // One band across the row: nothing in it may end the background.
    let band = theme.bg(Role::SelectedBg);
    let body = body.replace(RESET, &format!("{RESET}{band}\x1b[1m"));
    let line = format!(
        "{band}{}{} {}\x1b[1m{body}",
        theme.fg(Role::Accent),
        theme.glyph(Glyph::Rail),
        theme.fg(Role::Text),
    );
    let cut = text::truncate(&line, width, "");
    let open = cut.strip_suffix(RESET).unwrap_or(&cut);
    format!("{}{RESET}", text::pad_right(open, width))
}

/// The two rows under the list about the selected model: what it is and costs, and which
/// roles use it (`used` holds each role with its thinking level).
pub(super) fn detail(
    model: &ModelRow,
    used: &[(String, Option<String>)],
    width: usize,
    theme: &Theme,
) -> [Line; 2] {
    let separator = theme.glyph(Glyph::Separator);
    let mut facts: Vec<String> = Vec::new();
    if let Some(name) = model.name.as_deref().filter(|name| *name != model.id) {
        facts.push(text::sanitize(name));
    }
    facts.push(format!("{} ctx", crate::models::compact(model.context_window)));
    facts.push(format!("{} out", crate::models::compact(model.max_output)));
    match &model.cost {
        _ if model.login => facts.push("subscription".to_string()),
        Some(_) if is_free(model) => facts.push("free".to_string()),
        Some(cost) => facts.push(format!("{} per M", price_of(cost))),
        None => {}
    }
    if model.reasoning {
        facts.push(match model.thinking_levels.is_empty() {
            true => "thinking".to_string(),
            false => format!("thinking: {}", model.thinking_levels.join(" ")),
        });
    }
    if model.vision {
        facts.push("vision".to_string());
    }
    let ellipsis = theme.glyph(Glyph::Ellipsis);
    let first = theme.paint(
        Role::Muted,
        &text::truncate(&facts.join(separator), width, ellipsis),
    );
    let users: Vec<String> = used
        .iter()
        .map(|(role, level)| match level {
            Some(level) => format!("{role} {} {level}", theme.glyph(Glyph::RadioOn)),
            None => role.clone(),
        })
        .collect();
    let second = match users.is_empty() {
        true => theme.paint(Role::Muted, "used by no role"),
        false => format!(
            "{}{}",
            theme.paint(Role::Muted, "used by: "),
            text::truncate(&users.join(separator), width.saturating_sub(9), ellipsis)
        ),
    };
    [first, text::truncate(&second, width, "")]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use azazel_tui::GlyphLevel;

    use super::*;

    pub(crate) fn model(spec: &str) -> ModelRow {
        let (provider, id) = spec.split_once('/').unwrap();
        ModelRow {
            model: spec.to_string(),
            provider: provider.to_string(),
            id: id.to_string(),
            name: None,
            context_window: 200_000,
            max_output: 64_000,
            vision: true,
            reasoning: true,
            thinking_levels: vec!["low".into(), "medium".into(), "high".into()],
            cost: Some(ModelCost {
                input: 3.0,
                output: 15.0,
            }),
            usable: true,
            login: false,
        }
    }

    fn plain(line: &str) -> String {
        text::strip_ansi(line)
    }

    #[test]
    fn a_search_ranks_exact_names_above_contained_text_above_loose_matches() {
        let rows = vec![
            model("a/gpt-5-mini"),
            model("b/gpt"),
            model("c/my-gpt-x"),
            model("d/g-p-t"),
            model("e/claude"),
        ];
        let mut ranker = Ranker::default();
        let hits = ranker.hits("gpt", &rows);
        assert!(hits[4].is_none());
        let order = ranked(&hits, &[0, 1, 2, 3, 4]);
        assert_eq!(order[0], 1, "the exact id first");
        assert_eq!(*order.last().unwrap(), 3, "the loose match last");
        assert_eq!(order.len(), 4);
        // A model that costs nothing answers to `free`, and a name counts.
        let mut free = model("f/zero");
        free.cost = Some(ModelCost {
            input: 0.0,
            output: 0.0,
        });
        free.name = Some("Zero Large".into());
        let hits = ranker.hits("free", &[free.clone(), model("g/other")]);
        assert_eq!(ranked(&hits, &[0, 1]), [0]);
        let hits = ranker.hits("zero large", &[free]);
        assert_eq!(hits[0].map(|hit| hit.0), Some(0));
    }

    #[test]
    fn rows_fit_every_width_and_drop_price_then_output_then_context() {
        let theme = Theme::dark();
        let rows = [model("anthropic/claude-opus-5-5"), model("openai/gpt-5.6-luna")];
        let mut seen = Vec::new();
        for width in 20..=200 {
            let columns = Columns::of(rows.iter(), width);
            seen.push(columns);
            for selected in [false, true] {
                let tags = ["default".to_string(), "smol".to_string()];
                let marks = Marks {
                    current: true,
                    tags: &tags,
                    selected,
                    dim: false,
                };
                let line = row(&rows[0], &marks, &columns, width, &theme);
                assert!(text::width(&line) <= width, "{width}: {}", plain(&line));
            }
        }
        // Wide: every column. Then the price goes, then the output, then the context.
        assert!(
            seen.last()
                .is_some_and(|c| c.price > 0 && c.output > 0 && c.context > 0)
        );
        let first_without = |test: fn(&Columns) -> bool| seen.iter().rposition(|c| !test(c)).unwrap();
        let price = first_without(|c| c.price > 0);
        let output = first_without(|c| c.output > 0);
        let context = first_without(|c| c.context > 0);
        assert!(price > output && output > context, "{price} {output} {context}");
    }

    #[test]
    fn a_row_shows_the_columns_the_current_mark_and_the_tags_at_every_glyph_level() {
        for level in GlyphLevel::ALL {
            let theme = Theme::dark().with_glyphs(level);
            let model = model("anthropic/claude-opus-5-5");
            let columns = Columns::of([&model].into_iter(), 100);
            let tags = ["default".to_string()];
            let marks = Marks {
                current: true,
                tags: &tags,
                selected: false,
                dim: false,
            };
            let line = plain(&row(&model, &marks, &columns, 100, &theme));
            let bullet = theme.glyph(Glyph::Bullet);
            assert!(
                line.starts_with(&format!("  anthropic/claude-opus-5-5 {bullet} default")),
                "{line}"
            );
            let marks = format!("{}{}", theme.glyph(Glyph::Reasons), theme.glyph(Glyph::Vision));
            assert!(
                line.trim_end().ends_with(&format!("200k  64k  $3/$15  {marks}")),
                "{line}"
            );
            if level == GlyphLevel::Ascii {
                assert!(line.is_ascii(), "{line}");
            }
        }
        // Through a sign-in a model is not paid by the token.
        let mut login = model("anthropic/x");
        login.login = true;
        assert_eq!(price(&login), "subscription");
    }
}
