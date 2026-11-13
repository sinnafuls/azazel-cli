//! The comparisons: one function per registry, each naming the page and the name that is off.

use std::collections::{BTreeMap, BTreeSet};

use super::code::{CliCommand, Code};
use super::markdown::{Page, Table, code_spans};
use super::{Docs, Finding};

pub const CONFIGURATION: &str = "docs/configuration.md";
pub const TUI: &str = "docs/tui.md";
pub const TOOLS: &str = "docs/tools.md";
pub const HOOKS: &str = "docs/hooks.md";
pub const EXTENSIONS: &str = "docs/extensions.md";
pub const RPC: &str = "docs/rpc.md";
pub const RPC_SCHEMA: &str = "docs/rpc.schema.json";
pub const INDEX: &str = "docs/README.md";
pub const SKILL: &str = "skills/azazel-extensions/SKILL.md";
pub const CHANGELOG: &str = "CHANGELOG.md";
pub const FEATURES: &str = "FEATURES.md";
pub const AGENTS: &str = "AGENTS.md";
pub const CLAUDE: &str = "CLAUDE.md";

/// Names found on a page, each with the line it was found on.
type Named = BTreeMap<String, usize>;

/// Reports the names of one registry that only the code or only the page has. `what` is one
/// such name in words (`hook event`), `place` where the page lists them (`the events table`).
fn compare(
    out: &mut Vec<Finding>,
    page: &str,
    what: &str,
    place: &str,
    code: &BTreeSet<String>,
    docs: &Named,
) {
    for name in code.iter().filter(|name| !docs.contains_key(*name)) {
        out.push(Finding::page(
            page,
            format!("{what} `{name}` is in the code and missing from {place}"),
        ));
    }
    for (name, line) in docs.iter().filter(|(name, _)| !code.contains(*name)) {
        out.push(Finding::line(
            page,
            *line,
            format!("{what} `{name}` is in {place} and not in the code"),
        ));
    }
}

/// The page, or a finding that it is missing.
fn page<'a>(docs: &'a Docs, out: &mut Vec<Finding>, path: &str) -> Option<&'a Page> {
    let page = docs.page(path);
    if page.is_none() {
        out.push(Finding::page(path, "the page is missing".to_string()));
    }
    page
}

/// Whether a name is made of lower-case letters, digits and the given extra characters.
fn plain(name: &str, extra: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || extra.contains(c))
}

/// The plain names among the code spans of the cell `column` of every row.
fn column_names(table: &Table, column: usize, extra: &str) -> Named {
    let mut names = Named::new();
    for row in &table.rows {
        let cell = row.cells.get(column).map(String::as_str).unwrap_or_default();
        for span in code_spans(cell).into_iter().filter(|span| plain(span, extra)) {
            names.entry(span.to_string()).or_insert(row.line);
        }
    }
    names
}

/// A table or key as the settings tables write it (`[providers.<id>]`, `segments.<name>`,
/// `[models."<provider>/<id>"]`) as a path in which every name the user chooses is `*`.
fn setting_path(written: &str) -> String {
    let inner = written.trim_matches(|c| c == '[' || c == ']');
    let mut segments: Vec<String> = Vec::new();
    let (mut segment, mut quoted) = (String::new(), false);
    for c in inner.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                segment.push(c);
            }
            '.' if !quoted => segments.push(std::mem::take(&mut segment)),
            c => segment.push(c),
        }
    }
    segments.push(segment);
    segments
        .iter()
        .map(|segment| match segment.contains(['<', '"']) {
            true => "*",
            false => segment.as_str(),
        })
        .collect::<Vec<_>>()
        .join(".")
}

/// A path without the `*` it ends in: `statusline.segments.*` is the key `statusline.segments`.
fn canonical(path: &str) -> &str {
    let mut path = path;
    while let Some(shorter) = path.strip_suffix(".*") {
        path = shorter;
    }
    path
}

/// Whether a default as the docs write it is the default of the code: numbers by value, the
/// rest letter by letter without the spaces.
fn same_literal(written: &str, code: &str) -> bool {
    if let (Ok(a), Ok(b)) = (written.parse::<f64>(), code.parse::<f64>()) {
        return a == b;
    }
    let squeeze = |text: &str| text.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    squeeze(written) == squeeze(code)
}

/// The cell when it is one code span and nothing else.
fn only_span(cell: &str) -> Option<&str> {
    let spans = code_spans(cell);
    match spans.as_slice() {
        [span] if cell.trim().len() == span.len() + 2 => Some(span),
        _ => None,
    }
}

/// The settings of `prefix` (a table path, empty for all) that the code has, by canonical path.
fn settings_of<'a>(code: &'a Code, prefix: &str) -> BTreeMap<&'a str, &'a Option<String>> {
    code.config
        .iter()
        .filter(|(path, _)| prefix.is_empty() || path.starts_with(&format!("{prefix}.")))
        .map(|(path, default)| (canonical(path), default))
        .collect()
}

/// Compares documented settings, as `path -> (line, default cell)`, with those of the code.
fn compare_settings(
    out: &mut Vec<Finding>,
    page: &str,
    place: &str,
    code: &BTreeMap<&str, &Option<String>>,
    documented: &BTreeMap<String, (usize, Option<String>)>,
) {
    let names: BTreeSet<String> = code.keys().map(ToString::to_string).collect();
    let lines: Named = documented
        .iter()
        .map(|(path, (line, _))| (path.clone(), *line))
        .collect();
    compare(out, page, "setting", place, &names, &lines);
    for (path, (line, written)) in documented {
        if let (Some(written), Some(Some(default))) = (written, code.get(path.as_str()))
            && !same_literal(written, default)
        {
            out.push(Finding::line(
                page,
                *line,
                format!(
                    "setting `{path}` defaults to `{default}` in the code and the table says `{written}`"
                ),
            ));
        }
    }
}

/// Every table and key of the configuration has a row in `docs/configuration.md` with its
/// default, and every row names a key that exists.
pub fn config(docs: &Docs, code: &Code, out: &mut Vec<Finding>) {
    let Some(page) = page(docs, out, CONFIGURATION) else {
        return;
    };
    let mut documented: BTreeMap<String, (usize, Option<String>)> = BTreeMap::new();
    let tables = page.tables();
    for table in tables
        .iter()
        .filter(|table| table.headers.iter().take(2).eq(["Table", "Key"]))
    {
        let mut prefix: Option<String> = None;
        for row in &table.rows {
            if let Some(written) = row
                .cells
                .first()
                .and_then(|cell| code_spans(cell).into_iter().next())
            {
                prefix = Some(setting_path(written));
            }
            let (Some(prefix), Some(keys)) = (&prefix, row.cells.get(1)) else {
                continue;
            };
            let default = row.cells.get(2).and_then(|cell| only_span(cell));
            for key in code_spans(keys) {
                let path = format!("{prefix}.{}", setting_path(key));
                documented
                    .entry(canonical(&path).to_string())
                    .or_insert((row.line, default.map(str::to_string)));
            }
        }
    }
    compare_settings(
        out,
        CONFIGURATION,
        "the settings tables",
        &settings_of(code, ""),
        &documented,
    );
}

/// A page that repeats the keys of one table under a heading that names it (``## `[tools]`
/// configuration``) lists every key of that table.
pub fn page_settings(docs: &Docs, code: &Code, out: &mut Vec<Finding>) {
    let tables: BTreeSet<&str> = code
        .config
        .keys()
        .filter_map(|path| path.split('.').next())
        .collect();
    for page in docs.pages().filter(|page| page.path != CONFIGURATION) {
        for table in page.tables() {
            let named = code_spans(&table.heading)
                .into_iter()
                .filter_map(|span| span.strip_prefix('[')?.strip_suffix(']'))
                .find(|name| tables.contains(name));
            let Some(name) = named else { continue };
            if !table.headers.iter().eq(["Key", "Default", "Meaning"]) {
                continue;
            }
            let mut documented = BTreeMap::new();
            for row in &table.rows {
                let default = row.cells.get(1).and_then(|cell| only_span(cell));
                for key in code_spans(row.cells.first().map(String::as_str).unwrap_or_default()) {
                    let path = format!("{name}.{}", setting_path(key));
                    documented
                        .entry(canonical(&path).to_string())
                        .or_insert((row.line, default.map(str::to_string)));
                }
            }
            let place = format!("the table under \"{}\"", table.heading);
            compare_settings(out, &page.path, &place, &settings_of(code, name), &documented);
        }
    }
}

/// The list of trust-gated settings in `docs/configuration.md`.
pub fn trust_gated(docs: &Docs, code: &Code, out: &mut Vec<Finding>) {
    let Some(page) = page(docs, out, CONFIGURATION) else {
        return;
    };
    let start = "Gated settings:";
    let Some((line, text)) = page.paragraph(start) else {
        out.push(Finding::page(
            CONFIGURATION,
            format!("the paragraph that starts with `{start}` is missing"),
        ));
        return;
    };
    let tables: BTreeSet<&str> = code
        .config
        .keys()
        .filter_map(|path| path.split('.').next())
        .collect();
    // A setting is `table` or `table.key`; the paragraph also names values (`bypass`).
    let listed: Named = code_spans(&text)
        .into_iter()
        .filter(|span| plain(span, "_.") && (span.contains('.') || tables.contains(span)))
        .map(|span| (span.to_string(), line))
        .collect();
    let place = "the `Gated settings:` list";
    compare(
        out,
        CONFIGURATION,
        "trust-gated setting",
        place,
        &code.trust_gated,
        &listed,
    );
}

/// Hook events and the fields of a hook, on the hooks page, in the configuration page and in
/// the built-in skill.
pub fn hooks(docs: &Docs, code: &Code, out: &mut Vec<Finding>) {
    if let Some(page) = page(docs, out, HOOKS) {
        let tables = page.tables();
        let events: Named = tables
            .iter()
            .filter(|table| {
                table
                    .headers
                    .first()
                    .is_some_and(|header| header == "Config event")
            })
            .flat_map(|table| column_names(table, 0, "_"))
            .collect();
        compare(
            out,
            HOOKS,
            "hook event",
            "the events table",
            &code.hook_events,
            &events,
        );
        let fields: Named = tables
            .iter()
            .find(|table| table.headers.iter().eq(["Field", "Default", "Meaning"]))
            .map(|table| column_names(table, 0, "_"))
            .unwrap_or_default();
        let known: BTreeSet<String> = code
            .config
            .keys()
            .filter_map(|path| path.strip_prefix("hooks.*."))
            .map(str::to_string)
            .collect();
        compare(out, HOOKS, "hook field", "the fields table", &known, &fields);
    }
    if let Some(page) = page(docs, out, CONFIGURATION) {
        let start = "Hook events:";
        match page.paragraph(start) {
            Some((line, text)) => {
                let listed: Named = code_spans(&text)
                    .into_iter()
                    .map(|span| (span.to_string(), line))
                    .collect();
                let place = "the `Hook events:` list";
                compare(
                    out,
                    CONFIGURATION,
                    "hook event",
                    place,
                    &code.hook_events,
                    &listed,
                );
            }
            None => out.push(Finding::page(
                CONFIGURATION,
                format!("the paragraph that starts with `{start}` is missing"),
            )),
        }
    }
    if let Some(page) = page(docs, out, SKILL) {
        let events: Named = page
            .tables()
            .iter()
            .filter(|table| table.headers.iter().take(2).eq(["Event", "Fires"]))
            .flat_map(|table| column_names(table, 0, "_"))
            .collect();
        compare(
            out,
            SKILL,
            "hook event",
            "the hook events table",
            &code.hook_events,
            &events,
        );
    }
}

/// The action names of the key table in `docs/tui.md`.
pub fn keymap(docs: &Docs, code: &Code, out: &mut Vec<Finding>) {
    let Some(page) = page(docs, out, TUI) else {
        return;
    };
    let actions: Named = page
        .tables()
        .iter()
        .filter(|table| table.headers.get(1).is_some_and(|header| header == "Action name"))
        .flat_map(|table| column_names(table, 1, "_"))
        .collect();
    compare(
        out,
        TUI,
        "keymap action",
        "the key table",
        &code.key_actions,
        &actions,
    );
}

/// The `--flags` named in a piece of text, whole words only.
fn long_flags(text: &str) -> Vec<&str> {
    let mut flags = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("--") {
        let tail = &rest[at..];
        let end = tail[2..]
            .find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
            .map_or(tail.len(), |end| end + 2);
        let before = rest[..at].chars().next_back();
        if end > 2 && !before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '-') {
            flags.push(tail[..end].trim_end_matches('-'));
        }
        rest = &tail[end.max(2)..];
    }
    flags
}

/// The flags of `azazel` itself in the flags table, and the subcommands with their actions in
/// the subcommands table, both in `docs/configuration.md`.
pub fn cli_tables(docs: &Docs, code: &Code, out: &mut Vec<Finding>) {
    let Some(page) = page(docs, out, CONFIGURATION) else {
        return;
    };
    let tables = page.tables();
    let Some(root) = code.command(&[]) else { return };

    let mut flags = Named::new();
    for table in tables
        .iter()
        .filter(|table| table.headers.iter().eq(["Flag", "Meaning"]))
    {
        for row in &table.rows {
            let cell = row.cells.first().map(String::as_str).unwrap_or_default();
            for flag in code_spans(cell).into_iter().flat_map(long_flags) {
                flags.entry(flag.to_string()).or_insert(row.line);
            }
        }
    }
    let visible: BTreeSet<String> = root
        .flags
        .iter()
        .filter(|(_, flag)| !flag.hidden)
        .map(|(name, _)| name.clone())
        .collect();
    compare(out, CONFIGURATION, "flag", "the flags table", &visible, &flags);

    // `azazel <command>` to the line of its row and the actions the row lists.
    let mut rows: BTreeMap<String, (usize, Named)> = BTreeMap::new();
    for table in tables
        .iter()
        .filter(|table| table.headers.iter().take(2).eq(["Command", "Actions"]))
    {
        for row in &table.rows {
            let command = row
                .cells
                .first()
                .and_then(|cell| code_spans(cell).into_iter().next());
            let Some(command) = command else { continue };
            let actions: Named = code_spans(row.cells.get(1).map(String::as_str).unwrap_or_default())
                .into_iter()
                .map(|action| (action.to_string(), row.line))
                .collect();
            rows.insert(command.to_string(), (row.line, actions));
        }
    }
    let place = "the subcommands table";
    let listed: Named = rows
        .iter()
        .map(|(name, (line, _))| (name.clone(), *line))
        .collect();
    // A command has a row when it is a subcommand of `azazel` or has actions of its own.
    let expected: BTreeSet<String> = code
        .cli
        .iter()
        .filter(|command| !command.hidden && !command.path.is_empty())
        .filter(|command| command.path.len() == 1 || !command.subcommands.is_empty())
        .map(CliCommand::name)
        .collect();
    compare(out, CONFIGURATION, "subcommand", place, &expected, &listed);
    for command in code.cli.iter().filter(|command| !command.hidden) {
        let Some((_, actions)) = rows.get(&command.name()) else {
            continue;
        };
        let visible: BTreeSet<String> = command
            .actions()
            .into_iter()
            .filter(|action| {
                let mut path = command.path.clone();
                path.push(action.clone());
                code.command(&path).is_some_and(|sub| !sub.hidden)
            })
            .collect();
        let what = format!("action of `{}`", command.name());
        compare(out, CONFIGURATION, &what, place, &visible, actions);
    }
}

/// One word of a command line as a page shows it.
struct Token {
    text: String,
    quoted: bool,
    /// Stands for something the reader fills in: `<id>`, `[flags]`.
    placeholder: bool,
}

/// Splits a command line into words; quotes keep a word together, and the brackets, commas and
/// dots of prose around a word are dropped. Stops at a pipe, `&&`, `;` or a comment.
fn tokens(line: &str) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::new();
    let mut current: Option<Token> = None;
    let mut quote: Option<char> = None;
    for c in line.chars() {
        match quote {
            Some(open) if c == open => quote = None,
            Some(_) => {
                if let Some(token) = current.as_mut() {
                    token.text.push(c);
                }
            }
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                current.get_or_insert(Token {
                    text: String::new(),
                    quoted: true,
                    placeholder: false,
                });
            }
            None if c.is_whitespace() => out.extend(current.take()),
            None => current
                .get_or_insert(Token {
                    text: String::new(),
                    quoted: false,
                    placeholder: false,
                })
                .text
                .push(c),
        }
    }
    out.extend(current.take());
    let end = out.iter().position(|token| {
        !token.quoted
            && (matches!(token.text.as_str(), "|" | "&&" | "||" | ";") || token.text.starts_with('#'))
    });
    out.truncate(end.unwrap_or(out.len()));
    for token in out.iter_mut().filter(|token| !token.quoted) {
        let text = token.text.trim_start_matches(['[', '(']);
        let text = text.trim_end_matches([']', ')', ',', '.', ';', ':', '…']);
        // `[flags]` is a placeholder; `[--json]` is an optional flag and `<id>]` the end of one.
        token.placeholder = text.starts_with('<') || (token.text.starts_with('[') && !text.starts_with('-'));
        token.text = text.to_string();
    }
    out
}

/// Every `azazel …` command line a page shows: in code spans, and in fenced code that is shell
/// or has no language. A leading prompt (`$ `) and `NAME=value` settings are dropped.
fn invocations(page: &Page) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    let mut shell = false;
    let command = |text: &str| -> Option<String> {
        let mut text = text.trim();
        text = text.strip_prefix("$ ").unwrap_or(text);
        loop {
            let word = text.split_whitespace().next().unwrap_or_default();
            let assignment = word.split_once('=').is_some_and(|(name, _)| {
                !name.is_empty()
                    && name
                        .chars()
                        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
            });
            if !assignment {
                break;
            }
            text = text[word.len()..].trim_start();
        }
        (text == "azazel" || text.starts_with("azazel ")).then(|| text.to_string())
    };
    for (line, text, fenced) in page.numbered() {
        if let Some(language) = text.trim_start().strip_prefix("```") {
            shell = matches!(
                language.trim(),
                "" | "sh" | "bash" | "shell" | "console" | "powershell" | "text"
            );
            continue;
        }
        if fenced {
            if shell {
                found.extend(command(text).map(|command| (line, command)));
            }
        } else {
            found.extend(
                code_spans(text)
                    .into_iter()
                    .filter_map(command)
                    .map(|command| (line, command)),
            );
        }
    }
    found
}

/// Checks one command line a page shows against the command line the code defines: every
/// subcommand it names exists, and so does every flag, on the command it is given to.
fn check_invocation(code: &Code, page: &str, line: usize, shown: &str, out: &mut Vec<Finding>) {
    let Some(mut current) = code.command(&[]) else {
        return;
    };
    let words = tokens(shown);
    let mut positional = false;
    let mut skip = false;
    for token in words.iter().skip(1) {
        if std::mem::take(&mut skip) {
            continue;
        }
        let text = token.text.as_str();
        if token.placeholder {
            continue;
        }
        if token.quoted || text.is_empty() {
            positional = true;
            continue;
        }
        if text == "--" {
            break;
        }
        if let Some(flag) = text.strip_prefix("--") {
            let name = flag.split('=').next().unwrap_or_default();
            // `--no-*`, `--<flag>`: a pattern, not a flag.
            if !plain(name, "-") {
                continue;
            }
            match current.flags.get(&format!("--{name}")) {
                Some(known) => skip = known.takes_value && !flag.contains('='),
                None => out.push(Finding::line(
                    page,
                    line,
                    format!("`{shown}`: `{}` has no flag `--{name}`", current.name()),
                )),
            }
            continue;
        }
        if let Some(short) = text.strip_prefix('-') {
            let mut letters = short.chars();
            if let (Some(letter), None) = (letters.next(), letters.next())
                && letter.is_ascii_alphabetic()
            {
                match current.shorts.get(&letter) {
                    Some(takes_value) => skip = *takes_value,
                    None => out.push(Finding::line(
                        page,
                        line,
                        format!("`{shown}`: `{}` has no flag `-{letter}`", current.name()),
                    )),
                }
            }
            continue;
        }
        if positional {
            continue;
        }
        let below = current.subcommands.get(text).and_then(|name| {
            let mut path = current.path.clone();
            path.push(name.clone());
            code.command(&path)
        });
        match below {
            Some(command) => current = command,
            // `azazel` takes a prompt where a subcommand could stand; a command with actions
            // takes nothing else there.
            None if plain(text, "-") && !current.path.is_empty() && !current.subcommands.is_empty() => {
                out.push(Finding::line(
                    page,
                    line,
                    format!("`{shown}`: `{text}` is not an action of `{}`", current.name()),
                ));
                positional = true;
            }
            None => positional = true,
        }
    }
}

/// Whether a line of a page names `command` (`azazel logs`, also written `azazel [flags] logs`
/// or, for an action, with the action later on the line) and the flag, in its long or its
/// one-letter form.
fn names_flag(text: &str, command: &CliCommand, long: &str, short: Option<char>) -> bool {
    let text = text.replace("[flags] ", "");
    let words = || text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'));
    let top = format!(
        "azazel {}",
        command.path.first().map(String::as_str).unwrap_or_default()
    );
    let last = command.path.last().map(String::as_str).unwrap_or_default();
    let about = text.contains(&command.name()) || (text.contains(&top) && words().any(|word| word == last));
    let short = short.map(|letter| format!("-{letter}"));
    about
        && (long_flags(&text).contains(&long) || short.is_some_and(|short| words().any(|word| word == short)))
}

/// Every flag of every subcommand is named on a line of `docs/` together with its command, and
/// every command line the docs show uses subcommands and flags that exist.
pub fn cli_usage(docs: &Docs, code: &Code, out: &mut Vec<Finding>) {
    let pages: Vec<&Page> = docs
        .pages()
        .filter(|page| page.path.starts_with("docs/"))
        .collect();
    for command in code
        .cli
        .iter()
        .filter(|command| !command.hidden && !command.path.is_empty())
    {
        let name = command.name();
        for (long, flag) in command.flags.iter().filter(|(name, flag)| {
            !flag.hidden && !flag.global && !matches!(name.as_str(), "--help" | "--version")
        }) {
            let documented = pages.iter().any(|page| {
                let mut lines = page.lines.iter();
                lines.any(|line| names_flag(line, command, long, flag.short))
            });
            if !documented {
                out.push(Finding::page(
                    "docs",
                    format!(
                        "`{name} {long}` is not documented: no line of a page under docs/ names both `{name}` and \
                         `{long}`"
                    ),
                ));
            }
        }
    }
    // The changelog says what a release did, in the words of its day.
    for page in docs.pages().filter(|page| page.path != CHANGELOG) {
        for (line, shown) in invocations(page) {
            check_invocation(code, &page.path, line, &shown, out);
        }
    }
}

/// RPC commands, frames and UI request kinds in `docs/rpc.md`, and the generated schema file.
pub fn rpc(docs: &Docs, code: &Code, out: &mut Vec<Finding>) {
    match docs.text(RPC_SCHEMA) {
        Some(text) if text.replace("\r\n", "\n") == code.rpc_schema => {}
        Some(_) => out.push(Finding::page(
            RPC_SCHEMA,
            "the file is not what the protocol generates; run `cargo xtask rpc-schema`".to_string(),
        )),
        None => out.push(Finding::page(
            RPC_SCHEMA,
            "the file is missing; run `cargo xtask rpc-schema`".to_string(),
        )),
    }
    let Some(page) = page(docs, out, RPC) else {
        return;
    };
    let tables = page.tables();
    let typed = |columns: usize| -> Named {
        tables
            .iter()
            .filter(|table| table.headers.first().is_some_and(|header| header == "`type`"))
            .filter(|table| table.headers.len() == columns)
            .flat_map(first_names)
            .collect()
    };
    compare(
        out,
        RPC,
        "RPC command",
        "the command tables",
        &code.rpc_commands,
        &typed(4),
    );

    // A frame is a row of the events table or has a section of its own (### `roster`).
    let mut frames = typed(2);
    for heading in page.headings() {
        if let Some(name) = only_span(&heading.text) {
            frames.entry(name.to_string()).or_insert(heading.line);
        }
    }
    // `hello` and `response` are shown where the connection and the answers are described.
    let shown = |name: &str| {
        let compact = format!("\"type\":\"{name}\"");
        page.lines
            .iter()
            .any(|line| line.replace(' ', "").contains(&compact))
    };
    for name in code.rpc_frames.iter().filter(|name| shown(name)) {
        frames.entry(name.clone()).or_insert(0);
    }
    let place = "the events table and the frame sections";
    compare(out, RPC, "RPC frame", place, &code.rpc_frames, &frames);

    let kinds: Named = tables
        .iter()
        .filter(|table| table.headers.first().is_some_and(|header| header == "`kind`"))
        .flat_map(first_names)
        .collect();
    compare(
        out,
        RPC,
        "UI request kind",
        "the `kind` table",
        &code.rpc_ui_kinds,
        &kinds,
    );
}

/// The first code span of the first cell of every row, when it is a plain name.
fn first_names(table: &Table) -> Named {
    table
        .rows
        .iter()
        .filter_map(|row| {
            let name = code_spans(row.cells.first()?).into_iter().next()?;
            plain(name, "_").then(|| (name.to_string(), row.line))
        })
        .collect()
}

/// The functions a cell names: `azazel.fs.read(path)` in full, and `.write(path, content)` as
/// short for a function of the same table as the one before it.
fn luau_names(cell: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for span in code_spans(cell) {
        let end = span
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.'))
            .unwrap_or(span.len());
        let path = span[..end].trim_end_matches('.');
        if path.starts_with("azazel.") {
            names.push(path.to_string());
        } else if let (Some(short), Some(before)) = (path.strip_prefix('.'), names.last()) {
            let table = before.rsplit_once('.').map_or("azazel", |(table, _)| table);
            names.push(format!("{table}.{short}"));
        }
    }
    names
}

/// The Luau API and its events on one page: every `Function | Contract` table, and the event
/// tables whose second column is `second`.
fn luau_page(page: &Page, code: &Code, second: &str, out: &mut Vec<Finding>) {
    let tables = page.tables();
    let mut api = Named::new();
    for table in tables
        .iter()
        .filter(|table| table.headers.iter().eq(["Function", "Contract"]))
    {
        for row in &table.rows {
            for name in luau_names(row.cells.first().map(String::as_str).unwrap_or_default()) {
                api.entry(name).or_insert(row.line);
            }
        }
    }
    compare(
        out,
        &page.path,
        "Luau function",
        "the `Function | Contract` tables",
        &code.luau_api,
        &api,
    );
    let events: Named = tables
        .iter()
        .filter(|table| table.headers.iter().take(2).eq(["Event", second]))
        .flat_map(|table| column_names(table, 0, "_"))
        .collect();
    compare(
        out,
        &page.path,
        "Luau event",
        "the events table",
        &code.luau_events,
        &events,
    );
}

/// The Luau API in `docs/extensions.md` and in the built-in skill.
pub fn luau(docs: &Docs, code: &Code, out: &mut Vec<Finding>) {
    if let Some(page) = page(docs, out, EXTENSIONS) {
        luau_page(page, code, "Fields of `ev`", out);
    }
    if let Some(page) = page(docs, out, SKILL) {
        luau_page(page, code, "Replaceable", out);
    }
}

/// The slash commands a `Command` table names in its first column: `/name` up to its arguments.
/// `/skill:<name>` and the like are made at run time and are left out.
fn slash_names(table: &Table) -> Named {
    let mut names = Named::new();
    if table.headers.first().is_none_or(|header| header != "Command") {
        return names;
    }
    for row in &table.rows {
        for span in code_spans(row.cells.first().map(String::as_str).unwrap_or_default()) {
            let Some(rest) = span.strip_prefix('/') else {
                continue;
            };
            let name = rest.split([' ', '[', '<']).next().unwrap_or_default();
            if plain(name, "-") {
                names.entry(name.to_string()).or_insert(row.line);
            }
        }
    }
    names
}

/// Every slash command is in the tables of "Slash commands" in `docs/tui.md`, and every command
/// a `Command` table of any page names exists.
pub fn slash_commands(docs: &Docs, code: &Code, out: &mut Vec<Finding>) {
    let Some(tui) = page(docs, out, TUI) else {
        return;
    };
    let listed: Named = tui
        .tables()
        .iter()
        .filter(|table| table.section == "Slash commands")
        .flat_map(slash_names)
        .collect();
    let place = "the tables of \"Slash commands\"";
    compare(out, TUI, "slash command", place, &code.slash_commands, &listed);
    for page in docs.pages().filter(|page| page.path.starts_with("docs/")) {
        for (name, line) in page.tables().iter().flat_map(slash_names) {
            if !code.slash_commands.contains(&name) && !(page.path == TUI && listed.contains_key(&name)) {
                out.push(Finding::line(
                    &page.path,
                    line,
                    format!("slash command `/{name}` is in a command table and not in the code"),
                ));
            }
        }
    }
}

/// Every tool is in a `Tool` table of `docs/tools.md`, and every row names a tool.
pub fn tools(docs: &Docs, code: &Code, out: &mut Vec<Finding>) {
    let Some(page) = page(docs, out, TOOLS) else {
        return;
    };
    let listed: Named = page
        .tables()
        .iter()
        .filter(|table| table.headers.first().is_some_and(|header| header == "Tool"))
        .flat_map(|table| column_names(table, 0, "_"))
        .collect();
    compare(out, TOOLS, "tool", "the `Tool` tables", &code.tools, &listed);
}

/// Every page of `docs/` is linked from the index, and every relative link of every page leads
/// to a file that exists and, with an anchor, to a heading of that file.
pub fn links(docs: &Docs, out: &mut Vec<Finding>) {
    if let Some(index) = page(docs, out, INDEX) {
        let linked: BTreeSet<String> = index
            .links()
            .into_iter()
            .map(|(_, target)| target.split('#').next().unwrap_or_default().to_string())
            .collect();
        for page in docs.pages() {
            let Some(name) = page.path.strip_prefix("docs/") else {
                continue;
            };
            if page.path != INDEX && !linked.contains(name) {
                out.push(Finding::page(
                    INDEX,
                    format!("`{name}` is not linked from the index"),
                ));
            }
        }
    }
    for page in docs.pages() {
        let dir = page.path.rsplit_once('/').map_or("", |(dir, _)| dir);
        for (line, target) in page.links() {
            if target.contains("://") || target.starts_with("mailto:") {
                continue;
            }
            let (file, anchor) = match target.split_once('#') {
                Some((file, anchor)) => (file, Some(anchor)),
                None => (target.as_str(), None),
            };
            let path = match file.is_empty() {
                true => page.path.clone(),
                false => resolve(dir, file),
            };
            if !docs.exists(&path) {
                out.push(Finding::line(
                    &page.path,
                    line,
                    format!("the link `{target}` leads to `{path}`, which does not exist"),
                ));
                continue;
            }
            let (Some(anchor), Some(target_page)) = (anchor, docs.page_or_read(&path)) else {
                continue;
            };
            if !anchor.is_empty() && !target_page.anchors().iter().any(|known| known == anchor) {
                out.push(Finding::line(
                    &page.path,
                    line,
                    format!("the link `{target}` names a heading `#{anchor}` that `{path}` does not have"),
                ));
            }
        }
    }
}

/// `file` as seen from the directory `dir`, both relative to the workspace.
fn resolve(dir: &str, file: &str) -> String {
    let mut parts: Vec<&str> = dir.split('/').filter(|part| !part.is_empty()).collect();
    for part in file.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    parts.join("/")
}

/// The version after `marker` on the first line that has it.
fn version_after(page: &Page, marker: &str) -> Option<(usize, String)> {
    page.lines.iter().enumerate().find_map(|(index, line)| {
        let rest = &line[line.find(marker)? + marker.len()..];
        let version: String = rest
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        let version = version.trim_end_matches('.').to_string();
        (!version.is_empty()).then_some((index + 1, version))
    })
}

/// The changelog has `[Unreleased]` and a section for the version of the workspace; the feature
/// comparison says which release its azazel column was checked against, and that is the newest.
pub fn releases(docs: &Docs, code: &Code, out: &mut Vec<Finding>) {
    let Some(changelog) = page(docs, out, CHANGELOG) else {
        return;
    };
    let sections: Vec<String> = changelog
        .headings()
        .into_iter()
        .filter(|heading| heading.level == 2)
        .filter_map(|heading| Some(heading.text.strip_prefix('[')?.split(']').next()?.to_string()))
        .collect();
    if sections.first().is_none_or(|first| first != "Unreleased") {
        out.push(Finding::page(
            CHANGELOG,
            "the first section must be `## [Unreleased]`".to_string(),
        ));
    }
    if !sections.contains(&code.version) {
        out.push(Finding::page(
            CHANGELOG,
            format!(
                "there is no section `## [{}]` for the version in Cargo.toml",
                code.version
            ),
        ));
    }
    let newest = sections.iter().find(|section| *section != "Unreleased");
    let Some(features) = page(docs, out, FEATURES) else {
        return;
    };
    let marker = "azazel column as of ";
    match (version_after(features, marker), newest) {
        (Some((line, stamped)), Some(newest)) if stamped != *newest => out.push(Finding::line(
            FEATURES,
            line,
            format!(
                "the azazel column is stamped {stamped} and the newest release in CHANGELOG.md is {newest}: \
                 check every row against the code again, then change the stamp"
            ),
        )),
        (None, _) => out.push(Finding::page(
            FEATURES,
            format!("there is no line saying `{marker}<version>, checked <date>`"),
        )),
        _ => {}
    }
}

/// The contributor guide exists, `CLAUDE.md` points to it, and every `cargo xtask <task>` a page
/// other than the changelog names is a task.
pub fn contributor_guide(docs: &Docs, code: &Code, out: &mut Vec<Finding>) {
    page(docs, out, AGENTS);
    if let Some(claude) = page(docs, out, CLAUDE)
        && !claude.lines.iter().any(|line| line.contains(AGENTS))
    {
        out.push(Finding::page(CLAUDE, format!("the page must point to {AGENTS}")));
    }
    let marker = "cargo xtask ";
    for page in docs.pages().filter(|page| page.path != CHANGELOG) {
        for (line, text, _) in page.numbered() {
            let mut rest = text;
            while let Some(at) = rest.find(marker) {
                rest = &rest[at + marker.len()..];
                let task: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_lowercase() || *c == '-')
                    .collect();
                if !task.is_empty() && !code.xtasks.contains(&task) {
                    out.push(Finding::line(
                        &page.path,
                        line,
                        format!("`cargo xtask {task}` is not a task of xtask"),
                    ));
                }
            }
        }
    }
}
