//! Markdown to styled terminal lines, and incremental rendering of a message while it streams.

use std::sync::LazyLock;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

use crate::glyphs::Glyph;
use crate::render::{HistoryItem, Line};
use crate::term;
use crate::text;
use crate::theme::{Color, RESET, Role, Theme};

const BOLD: &str = "\x1b[1m";
const ITALIC: &str = "\x1b[3m";
const UNDERLINE: &str = "\x1b[4m";
const STRIKE: &str = "\x1b[9m";

/// Renders a Markdown document to lines no wider than `width`.
///
/// Supports headings, paragraphs, emphasis, strong, strikethrough, inline code, fenced and
/// indented code blocks (syntax-highlighted when the language is known), nested bullet, ordered
/// and task lists, block quotes, tables, links (followed by their URL when it differs from the
/// text) and horizontal rules. Links and bare addresses are clickable when the theme says the
/// terminal follows hyperlinks. Incomplete input, such as an unterminated fence, renders as far
/// as it goes.
pub fn render(source: &str, width: usize, theme: &Theme) -> Vec<Line> {
    write(source, width, theme, None)
        .into_iter()
        .filter_map(|item| match item {
            HistoryItem::Line(line) => Some(line),
            HistoryItem::Block { .. } => None,
        })
        .collect()
}

/// What a [`FenceHook`] makes of a fenced code block.
pub enum Fenced {
    /// Shown in place of the source: a diagram drawn as an image, for example.
    Replace(Vec<HistoryItem>),
    /// The source is shown as usual, with this line under it (why it could not be drawn).
    Note(String),
}

/// Called with the language and the source of each finished fenced block that stands on its
/// own (not inside a list or a quote). `None` leaves the block as it is.
pub type FenceHook<'h> = dyn FnMut(&str, &str) -> Option<Fenced> + 'h;

/// [`render`] for the transcript: `hook` may replace fenced blocks with something drawn.
pub fn render_blocks(
    source: &str,
    width: usize,
    theme: &Theme,
    hook: &mut FenceHook<'_>,
) -> Vec<HistoryItem> {
    write(source, width, theme, Some(hook))
}

fn write(source: &str, width: usize, theme: &Theme, hook: Option<&mut FenceHook<'_>>) -> Vec<HistoryItem> {
    // Subscript is enabled only so that a single `~` stays literal: without it `~100 files, ~3
    // lines` would be read as strikethrough.
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_SUBSCRIPT;
    let mut writer = Writer::new(width.max(1), theme, hook);
    for event in Parser::new_ext(source, options) {
        writer.event(event);
    }
    writer.finish()
}

enum Container {
    Quote,
    /// `next` is the number of the next item of an ordered list.
    List {
        next: Option<u64>,
    },
    /// `marker` leads the item's first line; later lines are indented by its width.
    Item {
        marker: String,
        used: bool,
    },
}

#[derive(Default)]
struct Table {
    rows: Vec<Vec<String>>,
    row: Vec<String>,
    header_rows: usize,
}

struct Writer<'a, 'h> {
    theme: &'a Theme,
    hook: Option<&'a mut FenceHook<'h>>,
    width: usize,
    out: Vec<HistoryItem>,
    containers: Vec<Container>,
    /// Styled text of the block being collected.
    inline: String,
    /// SGR codes of the inline styles currently open, outermost first.
    styles: Vec<String>,
    /// A blank line is owed before the next block.
    need_gap: bool,
    /// Language and source of the code block being collected.
    code: Option<(String, String)>,
    /// Destination and visible text of the links and images currently open.
    links: Vec<(String, String)>,
    table: Option<Table>,
}

/// Whether an address is one a terminal may be asked to open.
fn linkable(url: &str) -> bool {
    ["https://", "http://", "mailto:", "file://"]
        .iter()
        .any(|scheme| url.starts_with(scheme))
}

impl<'a, 'h> Writer<'a, 'h> {
    fn new(width: usize, theme: &'a Theme, hook: Option<&'a mut FenceHook<'h>>) -> Self {
        Writer {
            theme,
            hook,
            width,
            out: Vec::new(),
            containers: Vec::new(),
            inline: String::new(),
            styles: Vec::new(),
            need_gap: false,
            code: None,
            links: Vec::new(),
            table: None,
        }
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t)
            | Event::Html(t)
            | Event::InlineHtml(t)
            | Event::InlineMath(t)
            | Event::DisplayMath(t) => self.text(&t),
            Event::Code(t) => self.inline_code(&t),
            Event::FootnoteReference(t) => self.text(&format!("[^{t}]")),
            Event::SoftBreak => self.inline.push(' '),
            Event::HardBreak => self.inline.push('\n'),
            Event::Rule => self.rule(),
            Event::TaskListMarker(done) => self.inline.push_str(if done { "[x] " } else { "[ ] " }),
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph | Tag::HtmlBlock => self.begin_block(),
            Tag::Heading { level, .. } => {
                self.begin_block();
                self.push_style(BOLD.to_string());
                if matches!(level, HeadingLevel::H1 | HeadingLevel::H2) {
                    self.push_style(self.theme.fg(Role::Accent));
                }
            }
            Tag::BlockQuote(_) => {
                self.begin_block();
                self.containers.push(Container::Quote);
            }
            Tag::CodeBlock(kind) => {
                self.begin_block();
                let language = match &kind {
                    CodeBlockKind::Fenced(info) => info.split([' ', ',', '\t']).next().unwrap_or_default(),
                    CodeBlockKind::Indented => "",
                };
                self.code = Some((language.to_string(), String::new()));
            }
            Tag::List(start) => {
                self.flush_inline();
                if matches!(self.containers.last(), Some(Container::Item { .. })) {
                    // A nested list continues its item without a blank line.
                    self.need_gap = false;
                } else {
                    self.gap();
                }
                self.containers.push(Container::List { next: start });
            }
            Tag::Item => {
                self.flush_inline();
                let marker = self.next_marker();
                self.containers.push(Container::Item { marker, used: false });
                self.need_gap = false;
            }
            Tag::Table(_) => {
                self.begin_block();
                self.table = Some(Table::default());
            }
            Tag::Emphasis => self.push_style(ITALIC.to_string()),
            Tag::Strong => self.push_style(BOLD.to_string()),
            Tag::Strikethrough => self.push_style(STRIKE.to_string()),
            Tag::Subscript => self.inline.push('~'),
            Tag::Link { dest_url, .. } => {
                self.open_link(&dest_url);
                self.push_style(UNDERLINE.to_string());
            }
            Tag::Image { dest_url, .. } => {
                self.open_link(&dest_url);
                self.inline.push_str("[image: ");
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::HtmlBlock => self.flush_inline(),
            TagEnd::Heading(_) => {
                self.styles.clear();
                self.flush_inline();
            }
            TagEnd::BlockQuote(_) | TagEnd::List(_) => {
                self.flush_inline();
                self.containers.pop();
                self.need_gap = true;
            }
            TagEnd::CodeBlock => {
                if let Some((language, source)) = self.code.take() {
                    self.code_block(&language, &source);
                }
                self.need_gap = true;
            }
            TagEnd::Item => {
                self.flush_inline();
                if matches!(self.containers.last(), Some(Container::Item { used: false, .. })) {
                    // An empty item still shows its marker.
                    self.emit_line(String::new());
                }
                self.containers.pop();
            }
            TagEnd::TableCell => {
                let cell = self.take_inline();
                if let Some(table) = &mut self.table {
                    table.row.push(cell.trim().to_string());
                }
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                if let Some(table) = &mut self.table {
                    let row = std::mem::take(&mut table.row);
                    table.rows.push(row);
                    if matches!(tag, TagEnd::TableHead) {
                        table.header_rows = table.rows.len();
                    }
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.table_block(&table);
                }
                self.need_gap = true;
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => self.pop_style(),
            TagEnd::Subscript => self.inline.push('~'),
            TagEnd::Link => {
                self.pop_style();
                self.close_link();
            }
            TagEnd::Image => {
                self.inline.push(']');
                self.close_link();
            }
            _ => {}
        }
    }

    fn text(&mut self, raw: &str) {
        if let Some((_, source)) = &mut self.code {
            source.push_str(raw);
            return;
        }
        let clean = text::sanitize(raw);
        match self.links.last_mut() {
            Some((_, shown)) => {
                shown.push_str(&clean);
                self.inline.push_str(&clean);
            }
            None => self.plain_text(&clean),
        }
    }

    /// Text outside any link: an address written out in it becomes a link of its own.
    fn plain_text(&mut self, clean: &str) {
        let mut rest = clean;
        while self.theme.links()
            && let Some(found) = text::find_link(rest)
        {
            self.inline.push_str(&rest[..found.start]);
            let url = &rest[found.clone()];
            self.inline.push_str(&self.theme.link(url, url));
            rest = &rest[found.end..];
        }
        self.inline.push_str(rest);
    }

    /// Starts a link or an image: what follows is clickable when the terminal follows links.
    fn open_link(&mut self, destination: &str) {
        if self.theme.links() && linkable(destination) {
            self.inline.push_str(&term::link_start(destination));
        }
        self.links.push((destination.to_string(), String::new()));
    }

    fn inline_code(&mut self, raw: &str) {
        let clean = text::sanitize(raw);
        if let Some((_, shown)) = self.links.last_mut() {
            shown.push_str(&clean);
        }
        self.inline.push_str(&self.theme.fg(Role::Accent));
        self.inline.push_str(&clean);
        self.restyle();
    }

    /// Appends the URL of the link or image that just ended unless its text already shows it.
    fn close_link(&mut self) {
        let Some((destination, shown)) = self.links.pop() else {
            return;
        };
        let shown = shown.trim();
        let repeats = shown == destination || destination.strip_prefix("mailto:") == Some(shown);
        if !destination.is_empty() && !repeats {
            let url = format!(" ({})", text::sanitize(&destination).replace('\n', " "));
            self.inline.push_str(RESET);
            self.inline.push_str(&self.theme.paint(Role::Muted, &url));
            self.restyle();
        }
        if self.theme.links() && linkable(&destination) {
            self.inline.push_str(text::LINK_END);
        }
    }

    fn push_style(&mut self, code: String) {
        self.inline.push_str(&code);
        self.styles.push(code);
    }

    fn pop_style(&mut self) {
        self.styles.pop();
        self.restyle();
    }

    /// Resets all attributes and opens the active styles again.
    fn restyle(&mut self) {
        self.inline.push_str(RESET);
        for code in &self.styles {
            self.inline.push_str(code);
        }
    }

    /// Takes the collected inline text, leaving a buffer that starts with the open styles.
    fn take_inline(&mut self) -> String {
        let fresh = self.styles.concat();
        std::mem::replace(&mut self.inline, fresh)
    }

    fn begin_block(&mut self) {
        self.flush_inline();
        self.gap();
    }

    fn gap(&mut self) {
        if !self.need_gap {
            return;
        }
        self.need_gap = false;
        let mut line = String::new();
        if self.prefix_fits() {
            for container in &self.containers {
                match container {
                    Container::Quote => {
                        line.push_str(&self.theme.paint(Role::Border, self.theme.glyph(Glyph::TreeLine)));
                        line.push(' ');
                    }
                    Container::Item { marker, .. } => {
                        line.extend(std::iter::repeat_n(' ', text::width(marker)))
                    }
                    Container::List { .. } => {}
                }
            }
        }
        self.out.push(line.trim_end().to_string().into());
    }

    /// Wraps the collected inline text into the space left by the open containers.
    fn flush_inline(&mut self) {
        let inline = self.take_inline();
        if text::strip_ansi(&inline).trim().is_empty() {
            return;
        }
        let avail = self.avail();
        for line in text::wrap(inline.trim_end_matches('\n'), avail) {
            self.emit_line(line);
        }
        self.need_gap = true;
    }

    fn prefix_width(&self) -> usize {
        self.containers
            .iter()
            .map(|container| match container {
                Container::Quote => 2,
                Container::Item { marker, .. } => text::width(marker),
                Container::List { .. } => 0,
            })
            .sum()
    }

    /// Deeply nested containers can leave no room for text; their prefixes are then dropped.
    fn prefix_fits(&self) -> bool {
        self.prefix_width() < self.width
    }

    fn avail(&self) -> usize {
        if self.prefix_fits() {
            self.width - self.prefix_width()
        } else {
            self.width
        }
    }

    /// Emits one line of block content behind the quote bars and list markers of its containers.
    fn emit_line(&mut self, content: String) {
        if !self.prefix_fits() {
            for container in &mut self.containers {
                if let Container::Item { used, .. } = container {
                    *used = true;
                }
            }
            self.out.push(content.into());
            return;
        }
        let mut line = String::new();
        for container in &mut self.containers {
            match container {
                Container::Quote => {
                    line.push_str(&self.theme.paint(Role::Border, self.theme.glyph(Glyph::TreeLine)));
                    line.push(' ');
                }
                Container::Item { marker, used } => {
                    if *used {
                        line.extend(std::iter::repeat_n(' ', text::width(marker)));
                    } else {
                        line.push_str(&self.theme.paint(Role::Muted, marker));
                        *used = true;
                    }
                }
                Container::List { .. } => {}
            }
        }
        line.push_str(&content);
        self.out.push(line.into());
    }

    fn next_marker(&mut self) -> String {
        let depth = self
            .containers
            .iter()
            .filter(|c| matches!(c, Container::List { .. }))
            .count();
        match self.containers.last_mut() {
            Some(Container::List { next: Some(number) }) => {
                let marker = format!("{number}. ");
                *number += 1;
                marker
            }
            _ => {
                let bullets = self.theme.glyphs().bullets();
                format!("{} ", bullets[depth.saturating_sub(1) % bullets.len()])
            }
        }
    }

    fn rule(&mut self) {
        self.begin_block();
        let dash = self.theme.glyphs().border().horizontal;
        let line = self.theme.paint(Role::Border, &dash.repeat(self.avail()));
        self.emit_line(line);
        self.need_gap = true;
    }

    fn code_block(&mut self, language: &str, source: &str) {
        // Only a block that stands on its own can become something else: inside a list or a
        // quote it keeps its place in the text.
        let made = match &mut self.hook {
            Some(hook) if self.containers.is_empty() => hook(language, source),
            _ => None,
        };
        match made {
            Some(Fenced::Replace(items)) => self.out.extend(items),
            Some(Fenced::Note(note)) => {
                self.code_lines(language, source);
                let note = self.theme.paint(Role::Muted, &text::sanitize(&note));
                for line in text::wrap(&note, self.avail()) {
                    self.emit_line(line);
                }
            }
            None => self.code_lines(language, source),
        }
    }

    fn code_lines(&mut self, language: &str, source: &str) {
        let avail = self.avail();
        let source = text::sanitize(source);
        let source = source.strip_suffix('\n').unwrap_or(&source);
        let highlighted = highlight(language, source, self.theme);

        // One column of padding on each side; too narrow for that, the code is shown bare.
        if avail < 3 {
            for line in &highlighted {
                for piece in text::wrap_hard(line, avail) {
                    self.emit_line(piece);
                }
            }
            return;
        }
        let pieces: Vec<String> = highlighted
            .iter()
            .flat_map(|line| text::wrap_hard(line, avail - 2))
            .collect();
        let block_width = pieces.iter().map(|piece| text::width(piece)).max().unwrap_or(0);
        let background = self.theme.bg(Role::CodeBg);
        for piece in pieces {
            let padding = " ".repeat(block_width - text::width(&piece) + 1);
            // A styled piece ends with a reset, so the background is opened again for the padding.
            self.emit_line(format!("{background} {piece}{background}{padding}{RESET}"));
        }
    }

    fn table_block(&mut self, table: &Table) {
        let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        if columns == 0 {
            return;
        }
        let avail = self.avail();
        let natural: Vec<usize> = (0..columns)
            .map(|c| {
                table
                    .rows
                    .iter()
                    .filter_map(|row| row.get(c))
                    .map(|cell| text::width(cell))
                    .max()
                    .unwrap_or(0)
                    .max(1)
            })
            .collect();
        // Each column costs its content plus a space on both sides and a bar; one more bar closes
        // the row.
        let frame = 3 * columns + 1;
        let Some(widths) = fit_columns(&natural, avail.saturating_sub(frame)) else {
            // No room for a grid: one wrapped line per row.
            for row in &table.rows {
                for line in text::wrap(&row.join(" | "), avail) {
                    self.emit_line(line);
                }
            }
            return;
        };

        let theme = self.theme;
        let grid = theme.glyphs().table();
        let bar = theme.paint(Role::Border, grid.vertical);
        let rule = |[left, middle, right]: [&str; 3]| {
            let segments: Vec<String> = widths.iter().map(|w| grid.horizontal.repeat(w + 2)).collect();
            theme.paint(Role::Border, &format!("{left}{}{right}", segments.join(middle)))
        };

        self.emit_line(rule(grid.top));
        for (index, row) in table.rows.iter().enumerate() {
            let header = index < table.header_rows;
            let cells: Vec<Vec<String>> = widths
                .iter()
                .enumerate()
                .map(|(c, w)| text::wrap(row.get(c).map_or("", String::as_str), *w))
                .collect();
            let height = cells.iter().map(Vec::len).max().unwrap_or(1);
            for r in 0..height {
                let mut line = bar.clone();
                for (cell, w) in cells.iter().zip(&widths) {
                    let piece = text::pad_right(cell.get(r).map_or("", String::as_str), *w);
                    line.push(' ');
                    if header {
                        // Styles inside the cell end with a reset; bold is opened again after each.
                        line.push_str(BOLD);
                        line.push_str(&piece.replace(RESET, "\x1b[0;1m"));
                        line.push_str(RESET);
                    } else {
                        line.push_str(&piece);
                    }
                    line.push(' ');
                    line.push_str(&bar);
                }
                self.emit_line(line);
            }
            if index + 1 == table.header_rows && index + 1 < table.rows.len() {
                self.emit_line(rule(grid.middle));
            }
        }
        self.emit_line(rule(grid.bottom));
    }

    fn finish(mut self) -> Vec<HistoryItem> {
        self.flush_inline();
        let width = self.width;
        self.out
            .into_iter()
            .map(|item| match item {
                HistoryItem::Line(line) if text::width(&line) > width => {
                    HistoryItem::Line(text::truncate(&line, width, ""))
                }
                item => item,
            })
            .collect()
    }
}

/// Column widths that fit `budget` columns in total: natural widths when they fit, otherwise the
/// widest columns are narrowed first. `None` when not even one column each is available.
fn fit_columns(natural: &[usize], budget: usize) -> Option<Vec<usize>> {
    if budget < natural.len() {
        return None;
    }
    if natural.iter().sum::<usize>() <= budget {
        return Some(natural.to_vec());
    }
    let capped_total = |cap: usize| natural.iter().map(|n| (*n).min(cap)).sum::<usize>();
    let mut cap = budget / natural.len();
    while capped_total(cap + 1) <= budget {
        cap += 1;
    }
    let mut widths: Vec<usize> = natural.iter().map(|n| (*n).min(cap)).collect();
    let mut spare = budget - widths.iter().sum::<usize>();
    for (width, natural) in widths.iter_mut().zip(natural) {
        if spare == 0 {
            break;
        }
        if *width < *natural {
            *width += 1;
            spare -= 1;
        }
    }
    Some(widths)
}

struct Assets {
    syntaxes: SyntaxSet,
    themes: ThemeSet,
}

/// Syntax definitions and colour themes, loaded the first time a code block names a language.
static ASSETS: LazyLock<Assets> = LazyLock::new(|| Assets {
    syntaxes: SyntaxSet::load_defaults_newlines(),
    themes: ThemeSet::load_defaults(),
});

/// Splits code into lines and colours them when the language is known. Each line carries
/// foreground colours only and no reset, so a background applied around it survives.
fn highlight(language: &str, source: &str, theme: &Theme) -> Vec<String> {
    let coloured = match language {
        "" => None,
        _ => ASSETS
            .syntaxes
            .find_syntax_by_token(language)
            .and_then(|syntax| paint(syntax, source, theme)),
    };
    coloured.unwrap_or_else(|| source.split('\n').map(str::to_string).collect())
}

/// The lines of `source` coloured as the kind of file `path` names by its name or extension,
/// like [`highlight`]; `None` when the kind is not known.
pub(crate) fn highlight_file(path: &str, source: &str, theme: &Theme) -> Option<Vec<String>> {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let extension = name.rsplit_once('.').map_or("", |(_, extension)| extension);
    let extension = extension.to_ascii_lowercase();
    // Close relatives of a language the bundled definitions know.
    let known = match extension.as_str() {
        "ts" | "mts" | "cts" | "tsx" | "jsx" | "mjs" | "cjs" => "js",
        "jsonc" | "json5" => "json",
        "pyi" => "py",
        "hh" | "hxx" => "hpp",
        other => other,
    };
    let syntaxes = &ASSETS.syntaxes;
    let syntax = syntaxes
        .find_syntax_by_extension(name)
        .or_else(|| syntaxes.find_syntax_by_extension(known))
        .filter(|syntax| syntax.name != "Plain Text")?;
    paint(syntax, source, theme)
}

fn paint(syntax: &SyntaxReference, source: &str, theme: &Theme) -> Option<Vec<String>> {
    let assets: &Assets = &ASSETS;
    let theme_name = if theme.is_light() {
        "InspiredGitHub"
    } else {
        "base16-ocean.dark"
    };
    let colors = assets.themes.themes.get(theme_name)?;

    let mut highlighter = HighlightLines::new(syntax, colors);
    let mut lines = Vec::new();
    for raw in source.split('\n') {
        // The syntax definitions are the newline-aware set and expect each line to end with one.
        let with_newline = format!("{raw}\n");
        let regions = highlighter.highlight_line(&with_newline, &assets.syntaxes).ok()?;
        let mut line = String::new();
        for (style, piece) in regions {
            let piece = piece.trim_end_matches('\n');
            if piece.is_empty() {
                continue;
            }
            let fg = style.foreground;
            let bold = style.font_style.contains(FontStyle::BOLD);
            let italic = style.font_style.contains(FontStyle::ITALIC);
            line.push_str(&theme.fg_color(Color::Rgb(fg.r, fg.g, fg.b)));
            if bold {
                line.push_str(BOLD);
            }
            if italic {
                line.push_str(ITALIC);
            }
            line.push_str(piece);
            if bold {
                line.push_str("\x1b[22m");
            }
            if italic {
                line.push_str("\x1b[23m");
            }
        }
        lines.push(line);
    }
    Some(lines)
}

/// Renders a Markdown message while it is still arriving.
///
/// Text is pushed as it streams. Top-level blocks that can no longer change are handed out once by
/// [`take_finished`](Self::take_finished), ready to be written to scrollback, while
/// [`tail`](Self::tail) renders the unfinished remainder for the live region.
#[derive(Debug, Clone, Default)]
pub struct StreamingMarkdown {
    /// Source that has not been handed out yet.
    pending: String,
    /// Whether any lines were handed out, so that later output starts with a separating blank.
    emitted: bool,
}

impl StreamingMarkdown {
    pub fn new() -> StreamingMarkdown {
        StreamingMarkdown::default()
    }

    pub fn push(&mut self, delta: &str) {
        self.pending.push_str(delta);
    }

    /// Whether no unfinished text is left to show.
    pub fn is_empty(&self) -> bool {
        self.pending.trim().is_empty()
    }

    /// Returns the rendered lines of the blocks completed since the last call. A block is complete
    /// when a blank line outside a code fence follows it and the next block has begun. An indented
    /// line after the blank (a list item's continuation) keeps the block open, and so does another
    /// list item after a list, so that a list is always rendered whole.
    ///
    /// Blocks are rendered on their own: a reference-style link whose definition arrives in a
    /// later block shows as written.
    ///
    /// `hook` may replace a finished fenced block with something drawn, as in [`render_blocks`].
    pub fn take_finished(
        &mut self,
        width: usize,
        theme: &Theme,
        hook: &mut FenceHook<'_>,
    ) -> Vec<HistoryItem> {
        let cut = finished_len(&self.pending);
        if cut == 0 {
            return Vec::new();
        }
        let finished: String = self.pending.drain(..cut).collect();
        let items = self.separated(render_blocks(&finished, width, theme, hook));
        self.emitted |= !items.is_empty();
        items
    }

    /// Renders the text that [`take_finished`](Self::take_finished) has not handed out yet.
    pub fn tail(&self, width: usize, theme: &Theme) -> Vec<Line> {
        self.separated(render(&self.pending, width, theme))
    }

    /// Ends the message: returns everything left and resets for the next message.
    pub fn finish(&mut self, width: usize, theme: &Theme, hook: &mut FenceHook<'_>) -> Vec<HistoryItem> {
        let rest = std::mem::take(&mut self.pending);
        let items = self.separated(render_blocks(&rest, width, theme, hook));
        self.emitted = false;
        items
    }

    fn separated<T: From<String>>(&self, mut rows: Vec<T>) -> Vec<T> {
        if self.emitted && !rows.is_empty() {
            rows.insert(0, String::new().into());
        }
        rows
    }
}

/// Byte length of the leading part of `source` made of complete top-level blocks.
fn finished_len(source: &str) -> usize {
    let mut fence: Option<(char, usize)> = None;
    let mut cut = 0usize;
    let mut position = 0usize;
    let mut seen_content = false;
    let mut blank_before = false;
    // The block being scanned contains a list item.
    let mut in_list = false;
    for line in source.split_inclusive('\n') {
        let complete = line.ends_with('\n');
        let body = line.trim_end_matches(['\n', '\r']);
        let trimmed = body.trim_start_matches([' ', '\t']);
        if let Some((marker, length)) = fence {
            if complete && closes_fence(trimmed, marker, length) {
                fence = None;
            }
        } else if trimmed.is_empty() {
            blank_before = complete && seen_content;
        } else {
            let list = list_start(body, complete);
            // Cutting between the items of a list would restart its numbering.
            let continues_list = in_list && !matches!(list, ListStart::No);
            if blank_before && !body.starts_with([' ', '\t']) && !continues_list {
                cut = position;
                in_list = false;
            }
            in_list |= matches!(list, ListStart::Yes);
            blank_before = false;
            seen_content = true;
            fence = opens_fence(trimmed);
        }
        position += line.len();
    }
    cut
}

enum ListStart {
    Yes,
    No,
    /// The line is still arriving and so far is only part of a marker, such as `1` or `-`.
    Undecided,
}

/// Whether a line begins a bullet or ordered list item.
fn list_start(line: &str, complete: bool) -> ListStart {
    let line = line.trim_start_matches(' ');
    let digits = line.len() - line.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    let after_marker = if digits > 0 {
        let rest = &line[digits..];
        if rest.is_empty() {
            return if complete {
                ListStart::No
            } else {
                ListStart::Undecided
            };
        }
        rest.strip_prefix(['.', ')'])
    } else {
        line.strip_prefix(['-', '*', '+'])
    };
    match after_marker.map(|rest| rest.chars().next()) {
        None => ListStart::No,
        Some(None) if complete => ListStart::Yes,
        Some(None) => ListStart::Undecided,
        Some(Some(' ' | '\t')) => ListStart::Yes,
        Some(Some(_)) => ListStart::No,
    }
}

fn fence_run(line: &str) -> Option<(char, usize)> {
    let marker = line.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let length = line.chars().take_while(|c| *c == marker).count();
    (length >= 3).then_some((marker, length))
}

fn opens_fence(line: &str) -> Option<(char, usize)> {
    let (marker, length) = fence_run(line)?;
    // A backtick fence cannot have backticks in its info string; that is inline code.
    let info = &line[length..];
    (marker != '`' || !info.contains('`')).then_some((marker, length))
}

fn closes_fence(line: &str, marker: char, length: usize) -> bool {
    fence_run(line).is_some_and(|(m, n)| m == marker && n >= length && line[n..].trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| text::strip_ansi(l)).collect()
    }

    fn no_hook(_: &str, _: &str) -> Option<Fenced> {
        None
    }

    /// The rows of items that are all text.
    fn lines(items: Vec<HistoryItem>) -> Vec<Line> {
        items
            .into_iter()
            .map(|item| match item {
                HistoryItem::Line(line) => line,
                HistoryItem::Block { .. } => panic!("a block where only text was expected"),
            })
            .collect()
    }

    fn rendered(source: &str, width: usize) -> Vec<String> {
        plain(&render(source, width, &Theme::dark()))
    }

    const DOCUMENT: &str = "# Title\n\nFirst paragraph with **bold** text and `code`.\n\n```rust\nlet a = 1;\n\nlet b = 2;\n```\n\n- item one\n- item two\n  - nested\n\n> quoted\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\nLast paragraph with a [link](https://example.com).\n";

    #[test]
    fn paragraphs_wrap_and_keep_inline_style_across_lines() {
        let lines = render("plain **bold words that wrap around** plain", 16, &Theme::dark());
        assert_eq!(plain(&lines), ["plain bold words", "that wrap around", "plain"]);
        assert!(lines[1].starts_with(BOLD), "bold continues on the wrapped line");
        assert!(!lines[2].contains(BOLD));
    }

    #[test]
    fn the_ascii_level_draws_lists_quotes_rules_and_tables_with_ascii_only() {
        let theme = Theme::dark().with_glyphs(crate::glyphs::GlyphLevel::Ascii);
        let source = "- a\n  - b\n    - c\n\n> quoted\n\n---\n\n| Name | Qty |\n|---|---|\n| kiwi | 12 |";
        assert_eq!(
            plain(&render(source, 14, &theme)),
            [
                "* a",
                "  - b",
                "    + c",
                "",
                "| quoted",
                "",
                "--------------",
                "",
                "+------+-----+",
                "| Name | Qty |",
                "+------+-----+",
                "| kiwi | 12  |",
                "+------+-----+",
            ]
        );
        for width in 1..40 {
            for line in plain(&render(source, width, &theme)) {
                assert!(line.is_ascii(), "width {width}: {line:?}");
            }
        }
    }

    #[test]
    fn links_show_their_url_when_it_differs() {
        let lines = rendered(
            "[docs](https://example.com/docs), <https://example.com> and ![logo](logo.png)",
            100,
        );
        assert_eq!(
            lines,
            ["docs (https://example.com/docs), https://example.com and [image: logo] (logo.png)"]
        );
    }

    #[test]
    fn lists_nest_and_number() {
        let lines = rendered("- a\n  - b\n    - c\n- d\n\n3. three\n4. four\n   continued", 40);
        assert_eq!(
            lines,
            [
                "• a",
                "  ◦ b",
                "    ▪ c",
                "• d",
                "",
                "3. three",
                "4. four continued"
            ]
        );
    }

    #[test]
    fn list_items_wrap_with_a_hanging_indent() {
        let lines = rendered(
            "1. first item with enough words to wrap\n2. second\n\n- [x] done\n- [ ] open",
            20,
        );
        assert_eq!(
            lines,
            [
                "1. first item with",
                "   enough words to",
                "   wrap",
                "2. second",
                "",
                "• [x] done",
                "• [ ] open"
            ]
        );
    }

    #[test]
    fn tables_align_columns() {
        let source = "| Name | Qty |\n|------|-----|\n| apple | 3 |\n| kiwi | 12 |";
        assert_eq!(
            rendered(source, 40),
            [
                "┌───────┬─────┐",
                "│ Name  │ Qty │",
                "├───────┼─────┤",
                "│ apple │ 3   │",
                "│ kiwi  │ 12  │",
                "└───────┴─────┘",
            ]
        );
    }

    #[test]
    fn tables_shrink_wrap_cells_and_fall_back_when_too_narrow() {
        let source = "| Name | Notes |\n|---|---|\n| apple | crisp and sweet |";
        assert_eq!(
            rendered(source, 19),
            [
                "┌───────┬─────────┐",
                "│ Name  │ Notes   │",
                "├───────┼─────────┤",
                "│ apple │ crisp   │",
                "│       │ and     │",
                "│       │ sweet   │",
                "└───────┴─────────┘",
            ]
        );
        assert_eq!(
            rendered(source, 7),
            ["Name |", "Notes", "apple |", "crisp", "and", "sweet"]
        );
        for width in 1..40 {
            for line in render(source, width, &Theme::dark()) {
                assert!(text::width(&line) <= width, "width {width}: {line:?}");
            }
        }
    }

    #[test]
    fn code_block_is_highlighted_and_width_bounded() {
        let source = "```rust\nfn main() { let answer = 42; }\n```";
        let theme = Theme::dark();
        let lines = render(source, 20, &theme);
        assert!(lines.len() >= 2, "a 30-column line must wrap at width 20");
        for line in &lines {
            assert!(text::width(line) <= 20, "{line:?}");
            assert!(
                line.starts_with(&theme.bg(Role::CodeBg)),
                "every code row has the code background"
            );
        }
        let colours: std::collections::BTreeSet<&str> = lines
            .iter()
            .flat_map(|l| l.split("\x1b["))
            .filter(|part| part.starts_with("38;2;"))
            .filter_map(|part| part.split('m').next())
            .collect();
        assert!(
            colours.len() >= 2,
            "expected several token colours, found {colours:?}"
        );
        // Hard wrapping moves no characters: without the padding the source reads straight through.
        let code = plain(&lines).concat().replace(' ', "");
        assert!(code.contains("fnmain(){letanswer=42;}"), "{code:?}");
    }

    #[test]
    fn unknown_language_and_downgraded_colours() {
        let lines = render("```no-such-language\nx = 1\n```", 40, &Theme::dark());
        assert_eq!(plain(&lines), [" x = 1 "]);
        assert!(!lines[0].contains("\x1b[38;"));

        let theme = Theme::dark().with_depth(crate::theme::ColorDepth::Ansi256);
        let lines = render("```rust\nlet x = 1;\n```", 40, &theme);
        assert!(lines[0].contains("\x1b[38;5;") && !lines[0].contains("\x1b[38;2;"));
    }

    #[test]
    fn incomplete_input_does_not_panic() {
        assert_eq!(rendered("```rust\nfn x() {", 40), [" fn x() { "]);
        for source in [
            "```",
            "~~~\n",
            "**bold",
            "[link](",
            "| a |\n|--",
            "> ",
            "- ",
            "1.",
            "#",
            "`x",
            "<div>",
            "",
        ] {
            for width in [1, 2, 3, 10] {
                for line in render(source, width, &Theme::dark()) {
                    assert!(text::width(&line) <= width, "{source:?} at {width}: {line:?}");
                }
            }
        }
        assert!(render("", 20, &Theme::dark()).is_empty());
        assert!(render("\n\n  \n", 20, &Theme::dark()).is_empty());
    }

    #[test]
    fn model_output_cannot_inject_escape_sequences() {
        // Plain paragraphs carry no styling of their own, so no control byte may survive at all,
        // however the parser splits the text around the brackets.
        let lines = render("before \x1b[2J\x1b[31mred\x07 after \x1bc.", 60, &Theme::dark());
        assert_eq!(lines.len(), 1);
        assert!(!lines[0].contains(['\x1b', '\x07']), "{:?}", lines[0]);
        assert!(
            lines[0].starts_with("before ") && lines[0].ends_with(" after ."),
            "{:?}",
            lines[0]
        );
    }

    #[test]
    fn every_line_respects_the_width() {
        for width in 1..70 {
            for line in render(DOCUMENT, width, &Theme::dark()) {
                assert!(text::width(&line) <= width, "width {width}: {line:?}");
            }
        }
    }

    #[test]
    fn streaming_hands_out_each_finished_block_once() {
        let theme = Theme::dark();
        let mut stream = StreamingMarkdown::new();
        stream.push("Hello wor");
        assert!(stream.take_finished(40, &theme, &mut no_hook).is_empty());
        assert_eq!(plain(&stream.tail(40, &theme)), ["Hello wor"]);

        stream.push("ld\n\n");
        assert!(
            stream.take_finished(40, &theme, &mut no_hook).is_empty(),
            "the next block has not begun"
        );
        stream.push("Next");
        assert_eq!(
            plain(&lines(stream.take_finished(40, &theme, &mut no_hook))),
            ["Hello world"]
        );
        assert!(
            stream.take_finished(40, &theme, &mut no_hook).is_empty(),
            "a block is handed out only once"
        );
        assert_eq!(plain(&stream.tail(40, &theme)), ["", "Next"]);

        assert_eq!(
            plain(&lines(stream.finish(40, &theme, &mut no_hook))),
            ["", "Next"]
        );
        assert!(stream.is_empty());
        assert!(stream.tail(40, &theme).is_empty());
        assert!(stream.finish(40, &theme, &mut no_hook).is_empty());

        // The next message starts without a leading separator.
        stream.push("Fresh");
        assert_eq!(plain(&lines(stream.finish(40, &theme, &mut no_hook))), ["Fresh"]);
    }

    #[test]
    fn streaming_keeps_fenced_code_and_list_continuations_whole() {
        let theme = Theme::dark();
        let mut stream = StreamingMarkdown::new();
        stream.push("```\nfirst\n\nsecond\n");
        assert!(
            stream.take_finished(40, &theme, &mut no_hook).is_empty(),
            "a blank line inside a fence ends nothing"
        );
        stream.push("```\n\nafter");
        assert_eq!(
            plain(&lines(stream.take_finished(40, &theme, &mut no_hook))),
            [" first  ", "        ", " second "]
        );

        let mut stream = StreamingMarkdown::new();
        stream.push("- item\n\n  continued\n\nnext");
        assert_eq!(
            plain(&lines(stream.take_finished(40, &theme, &mut no_hook))),
            ["• item", "", "  continued"]
        );
        assert_eq!(plain(&stream.tail(40, &theme)), ["", "next"]);
    }

    #[test]
    fn streaming_in_small_chunks_loses_nothing() {
        let theme = Theme::dark();
        let whole = plain(&render(DOCUMENT, 50, &theme));
        for chunk_size in [1, 3, 7, 64] {
            let mut stream = StreamingMarkdown::new();
            let mut collected: Vec<String> = Vec::new();
            let chars: Vec<char> = DOCUMENT.chars().collect();
            for chunk in chars.chunks(chunk_size) {
                stream.push(&chunk.iter().collect::<String>());
                collected.extend(plain(&lines(stream.take_finished(50, &theme, &mut no_hook))));
            }
            collected.extend(plain(&lines(stream.finish(50, &theme, &mut no_hook))));
            assert_eq!(collected, whole, "chunks of {chunk_size}");
            let first = collected
                .iter()
                .filter(|line| line.contains("First paragraph"))
                .count();
            assert_eq!(first, 1, "chunks of {chunk_size}");
        }
    }

    #[test]
    fn finished_prefix_ends_at_block_boundaries() {
        assert_eq!(finished_len("one\n\ntwo"), 5);
        assert_eq!(finished_len("one\n\n"), 0);
        assert_eq!(finished_len("one\ntwo"), 0);
        assert_eq!(finished_len("\n\none"), 0);
        assert_eq!(finished_len("a\n\nb\n\nc"), 6);
        assert_eq!(finished_len("```\na\n\nb\n```\n\nc"), 14);
        assert_eq!(finished_len("~~~\na\n\n```\n\nb"), 0);
        assert_eq!(finished_len("- a\n\n  b\n\nc"), 10);
        assert_eq!(finished_len("a\r\n\r\nb"), 5);
        // A line of non-breaking spaces is paragraph text, not a blank line.
        assert_eq!(finished_len("a\n\u{a0}\nb"), 0);
    }

    #[test]
    fn lists_are_never_cut_between_items() {
        assert_eq!(finished_len("1. a\n\n1. b\n\nend"), 12);
        assert_eq!(finished_len("- a\n\n- b"), 0);
        assert_eq!(finished_len("intro\n- a\n\n* b\n\n3) c"), 0);
        assert_eq!(finished_len("para\n\n- a"), 6);
        // A partial marker could still become an item, so the decision waits for more text.
        assert_eq!(finished_len("- a\n\n-"), 0);
        assert_eq!(finished_len("- a\n\n1"), 0);
        assert_eq!(finished_len("- a\n\n12."), 0);
        assert_eq!(finished_len("- a\n\n-x"), 5);
        assert_eq!(finished_len("- a\n\n1\n\nz"), 8);
        assert_eq!(finished_len("- a\n\n---\n"), 5);

        let theme = Theme::dark();
        let mut stream = StreamingMarkdown::new();
        stream.push("1. one\n\n1. two\n\n1. three\n\ndone");
        assert_eq!(
            plain(&lines(stream.take_finished(40, &theme, &mut no_hook))),
            ["1. one", "2. two", "3. three"]
        );
    }

    #[test]
    fn links_are_clickable_when_the_terminal_follows_them() {
        let theme = Theme::dark().with_links(true);
        let open = |url: &str| format!("\x1b]8;;{url}\x1b\\");
        let end = text::LINK_END;

        let lines = render("See [the docs](https://example.com/docs) now.", 60, &theme);
        assert_eq!(plain(&lines), ["See the docs (https://example.com/docs) now."]);
        // The text and the address after it are one link.
        assert!(
            lines[0].contains(&format!("{}\x1b[4mthe docs", open("https://example.com/docs"))),
            "{lines:?}"
        );
        assert_eq!(lines[0].matches(end).count(), 1);

        // An address written out is a link to itself; the full stop after it is not part of it.
        let lines = render(
            "Read https://example.com/a. Then `https://not.linked`.",
            80,
            &theme,
        );
        assert!(
            lines[0].contains(&format!(
                "{}https://example.com/a{end}. Then",
                open("https://example.com/a")
            )),
            "{lines:?}"
        );
        assert!(!lines[0].contains(&open("https://not.linked")));

        // Wrapped over two rows, each row closes the link and the next opens it again.
        let lines = render("[one two three four](https://example.com)", 9, &theme);
        assert!(lines.len() > 1);
        for line in &lines {
            assert_eq!(line.matches("\x1b]8;;https").count(), 1, "{line:?}");
            assert_eq!(line.matches(end).count(), 1, "{line:?}");
            assert!(text::width(line) <= 9);
        }

        // Nothing but http, https, mailto and file is handed to the terminal.
        let lines = render("[run](javascript:alert(1)) [mail](mailto:a@b.c)", 80, &theme);
        assert!(!lines[0].contains("\x1b]8;;javascript"));
        assert!(lines[0].contains(&open("mailto:a@b.c")));

        // Without link support the output carries no OSC 8 at all.
        let plain_theme = Theme::dark();
        let lines = render(
            "See [the docs](https://example.com/docs) at https://x.y",
            80,
            &plain_theme,
        );
        assert!(!lines[0].contains("\x1b]8"));
    }

    #[test]
    fn a_hook_replaces_fenced_blocks_that_stand_on_their_own() {
        let theme = Theme::dark();
        let seen = std::cell::RefCell::new(Vec::new());
        let mut hook = |language: &str, source: &str| {
            seen.borrow_mut().push((language.to_string(), source.to_string()));
            match language {
                "mermaid" => Some(Fenced::Replace(vec![HistoryItem::Block {
                    rows: 4,
                    paint: "IMAGE".to_string(),
                }])),
                "broken" => Some(Fenced::Note("could not draw this: bad arrow".to_string())),
                _ => None,
            }
        };
        let source = "Before\n\n```mermaid\nflowchart LR\n  A-->B\n```\n\n```broken\nx\n```\n\n```rust\nlet a = 1;\n```\n\n- item\n  ```mermaid\n  nested\n  ```\n";
        let items = render_blocks(source, 40, &theme, &mut hook);
        let shown: Vec<String> = items
            .iter()
            .map(|item| match item {
                HistoryItem::Line(line) => text::strip_ansi(line).trim_end().to_string(),
                HistoryItem::Block { rows, paint } => format!("<{rows} rows: {paint}>"),
            })
            .collect();
        assert_eq!(
            shown,
            [
                "Before",
                "",
                "<4 rows: IMAGE>",
                "",
                " x",
                "could not draw this: bad arrow",
                "",
                " let a = 1;",
                "",
                "• item",
                "",
                "   nested",
            ]
        );
        // The block inside the list was not offered to the hook.
        let offered: Vec<String> = seen
            .borrow()
            .iter()
            .map(|(language, _)| language.clone())
            .collect();
        assert_eq!(offered, ["mermaid", "broken", "rust"]);
        assert_eq!(seen.borrow()[0].1, "flowchart LR\n  A-->B\n");
        // The plain renderer never sees a block: the source stays.
        assert!(
            plain(&render(source, 40, &theme))
                .iter()
                .any(|line| line.trim() == "flowchart LR")
        );

        // While it streams the block is source; once finished it is handed to the hook.
        let mut stream = StreamingMarkdown::new();
        stream.push("```mermaid\nflowchart LR\n");
        assert!(stream.take_finished(40, &theme, &mut hook).is_empty());
        assert_eq!(plain(&stream.tail(40, &theme)), [" flowchart LR "]);
        stream.push("```\n\nAfter");
        let finished = stream.take_finished(40, &theme, &mut hook);
        assert!(matches!(finished[..], [HistoryItem::Block { rows: 4, .. }]));
        assert_eq!(plain(&lines(stream.finish(40, &theme, &mut hook))), ["", "After"]);
    }
}
