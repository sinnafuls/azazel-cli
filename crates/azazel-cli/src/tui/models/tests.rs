#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use azazel_protocol::{ModelCost, ModelRow, ProviderRow, RoleRow, RoleSource, Roles, SaveTo};
use azazel_tui::mouse::{Mouse, MouseKind};
use azazel_tui::{Glyph, GlyphLevel, Glyphs, Role, Theme, text};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::screen::{Data, Models, Opening, Scope, Step};
use super::{FavouriteChange, Request};

fn model(spec: &str, levels: &[&str], usable: bool) -> ModelRow {
    let (provider, id) = spec.split_once('/').unwrap();
    ModelRow {
        model: spec.to_string(),
        provider: provider.to_string(),
        id: id.to_string(),
        name: None,
        context_window: 200_000,
        max_output: 64_000,
        vision: true,
        reasoning: !levels.is_empty(),
        thinking_levels: levels.iter().map(ToString::to_string).collect(),
        cost: Some(ModelCost {
            input: 3.0,
            output: 15.0,
        }),
        usable,
        login: false,
    }
}

fn provider(id: &str, usable: bool, models: usize) -> ProviderRow {
    ProviderRow {
        id: id.to_string(),
        name: id.to_string(),
        usable,
        login: false,
        source: usable.then(|| format!("{}_API_KEY", id.to_uppercase())),
        env: vec![format!("{}_API_KEY", id.to_uppercase())],
        oauth: id == "anthropic",
        models,
    }
}

fn role(name: &str, chain: &[&str], source: RoleSource, resolved: &[&str]) -> RoleRow {
    RoleRow {
        role: name.to_string(),
        chain: chain.iter().map(ToString::to_string).collect(),
        source,
        builtin: Vec::new(),
        resolved: resolved.iter().map(ToString::to_string).collect(),
        thinking: chain
            .first()
            .and_then(|first| first.rsplit_once(':'))
            .map(|(_, level)| level.to_string()),
    }
}

const OPUS: &str = "anthropic/claude-opus-5-5";
const HAIKU: &str = "anthropic/claude-haiku-4-5";
const LUNA: &str = "openai/gpt-5.6-luna";

fn roles(project: bool) -> Roles {
    Roles {
        roles: vec![
            role(
                "default",
                &["anthropic/claude-opus-5-5:high"],
                RoleSource::Home,
                &[OPUS],
            ),
            role("smol", &["@default"], RoleSource::Builtin, &[OPUS]),
            role("slow", &[LUNA, HAIKU], RoleSource::Home, &[LUNA, HAIKU]),
            role("judge", &[], RoleSource::Unset, &[]),
        ],
        presets: vec!["deep".to_string()],
        project,
    }
}

fn data() -> Data {
    Data {
        rows: vec![
            model(OPUS, &["low", "medium", "high"], true),
            model(HAIKU, &[], true),
            model(LUNA, &["low", "high"], true),
            model("openai/gpt-old", &[], true),
            model("groq/llama-4", &[], false),
        ],
        providers: vec![
            provider("anthropic", true, 2),
            provider("openai", true, 2),
            provider("groq", false, 1),
        ],
        roles: roles(true),
        current: Some(OPUS.to_string()),
        recent: vec![HAIKU.to_string()],
        cycle: vec!["@slow".to_string()],
    }
}

fn screen(opening: Opening) -> Models {
    Models::new(data(), &opening, &BTreeSet::new(), Glyphs::default())
}

fn on_role(role: &str) -> Models {
    screen(Opening {
        role: Some(role.to_string()),
        provider: None,
    })
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn press(screen: &mut Models, code: KeyCode) -> Option<Request> {
    match screen.on_key(&key(code)) {
        Step::Do(request) => Some(request),
        Step::Pending => None,
        Step::Close => panic!("the screen closed"),
    }
}

fn typed(screen: &mut Models, text: &str) {
    for c in text.chars() {
        press(screen, KeyCode::Char(c));
    }
}

fn shown(screen: &Models, width: usize, rows: usize) -> Vec<String> {
    let view = screen.view(width, rows, &Theme::dark());
    view.lines.iter().map(|line| text::strip_ansi(line)).collect()
}

fn set_role(role: &str, chain: &[&str]) -> Request {
    Request::SetRole {
        role: role.to_string(),
        chain: Some(chain.iter().map(ToString::to_string).collect()),
        save: SaveTo::Home,
    }
}

/// The session answered a change of `name` with `chain`.
fn answered(screen: &mut Models, name: &str, chain: &[&str]) {
    let mut roles = roles(true);
    let row = roles.roles.iter_mut().find(|row| row.role == name).unwrap();
    *row = role(name, chain, RoleSource::Home, &[]);
    screen.roles_changed(Ok(roles), None);
}

#[test]
fn every_width_and_glyph_level_fills_the_box_exactly() {
    for level in GlyphLevel::ALL {
        let theme = Theme::dark().with_glyphs(level);
        for scope in [Scope::Roles, Scope::All, Scope::Provider("groq".into())] {
            let mut screen = Models::new(data(), &Opening::default(), &BTreeSet::new(), theme.glyphs());
            screen.go_to_scope(scope.clone());
            for width in 60..=200 {
                let view = screen.view(width, 20, &theme);
                assert_eq!(view.lines.len(), 20, "{scope:?} {width}");
                for line in &view.lines {
                    assert!(text::width(line) <= width, "{scope:?} {width}: {line}");
                    if level == GlyphLevel::Ascii {
                        assert!(text::strip_ansi(line).is_ascii(), "{scope:?} {width}: {line}");
                    }
                }
                // The sidebar is there from 72 columns of text on.
                let plain = text::strip_ansi(&view.lines[2]);
                assert_eq!(plain.contains("Roles"), width >= 72, "{width}: {plain}");
            }
        }
    }
}

#[test]
fn the_model_list_puts_what_roles_use_and_recent_models_first() {
    let screen = screen(Opening::default());
    let lines = shown(&screen, 140, 20);
    let rows: Vec<&String> = lines[2..].iter().collect();
    let at = |what: &str| rows.iter().position(|row| row.contains(what)).unwrap();
    // `default` and `slow` name opus and luna; haiku was chosen lately; then the rule.
    assert!(at(OPUS) < at(LUNA) && at(LUNA) < at(HAIKU), "{lines:#?}");
    assert!(rows[at(HAIKU) + 1].contains('─'), "{lines:#?}");
    // The session is on opus, which `default` names.
    assert!(
        rows[at(OPUS)].contains(&format!("{OPUS} ● default")),
        "{lines:#?}"
    );
    // A provider without a credential is not among every model.
    assert!(
        !lines.iter().any(|line| line.contains("groq/llama")),
        "{lines:#?}"
    );
    // Under the list: what the selected model is and who uses it.
    assert!(
        lines
            .iter()
            .any(|line| line.contains("used by: default ◉ high · smol")),
        "{lines:#?}"
    );
}

#[test]
fn the_roles_view_shows_values_their_sources_fallbacks_and_the_cycle() {
    let screen = on_role("default");
    let lines = shown(&screen, 120, 20);
    let line = |what: &str| lines.iter().find(|line| line.contains(what)).unwrap().clone();
    let default = line("default  ");
    assert!(default.contains(&format!("● default  {OPUS}")), "{default}");
    assert!(default.trim_end().ends_with("◉ high      home"), "{default}");
    assert!(
        line(" smol ").contains(&format!("○ smol     auto → {OPUS}")),
        "{lines:#?}"
    );
    assert!(line(" smol ").trim_end().ends_with("built-in"));
    let slow = line(" slow ");
    assert!(slow.contains("⟳ 1"), "{slow}");
    assert!(line("↳").contains(&format!("↳ {HAIKU}")), "{lines:#?}");
    assert!(line(" judge ").contains("○ judge    none"), "{lines:#?}");
    assert!(line("+ New role").contains("+ New role…"));
    assert!(line("ctrl+p cycle:").contains("ctrl+p cycle: @slow"));
    // The sidebar counts the roles that have a value of their own.
    assert!(
        lines[2].contains("Roles") && lines[2].contains("2/4"),
        "{}",
        lines[2]
    );
}

#[test]
fn esc_goes_back_a_step_at_a_time() {
    let mut screen = on_role("smol");
    // Into assign mode, a search in it, a strip after a pick.
    press(&mut screen, KeyCode::Enter);
    assert!(screen.assign.is_some());
    typed(&mut screen, "luna");
    assert_eq!(
        press(&mut screen, KeyCode::Enter),
        Some(set_role("smol", &[LUNA]))
    );
    assert!(screen.strip.is_some(), "the thinking strip follows the pick");
    press(&mut screen, KeyCode::Esc);
    assert!(screen.strip.is_none());
    assert_eq!(screen.scope, Scope::Roles);

    // A search, then assign mode on top of it.
    let mut screen = on_role("smol");
    press(&mut screen, KeyCode::Char('f'));
    typed(&mut screen, "opus");
    assert!(screen.assign.is_some() && !screen.query.is_empty());
    press(&mut screen, KeyCode::Esc);
    assert!(screen.assign.is_none() && screen.scope == Scope::Roles);
    // The search outlives assign mode, then goes; then the screen closes.
    assert!(!screen.query.is_empty());
    press(&mut screen, KeyCode::Esc);
    assert!(screen.query.is_empty());
    assert!(matches!(screen.on_key(&key(KeyCode::Esc)), Step::Close));
}

#[test]
fn assigning_a_model_keeps_the_fallbacks_and_offers_its_thinking_levels() {
    let mut screen = on_role("slow");
    press(&mut screen, KeyCode::Enter);
    // The list opens on the model the role has now.
    assert_eq!(screen.selected_model().unwrap().model, LUNA);
    assert!(shown(&screen, 140, 20)[0].contains("Choosing the model of SLOW"));
    typed(&mut screen, "opus");
    assert_eq!(
        press(&mut screen, KeyCode::Enter),
        Some(set_role("slow", &[OPUS, HAIKU]))
    );
    answered(&mut screen, "slow", &[OPUS, HAIKU]);
    // `inherit`, then the model's own levels; enter on one writes it as the suffix.
    let strip = shown(&screen, 140, 20).pop().unwrap();
    assert!(strip.contains("inherit") && strip.contains("medium"), "{strip}");
    press(&mut screen, KeyCode::Right);
    press(&mut screen, KeyCode::Right);
    assert_eq!(
        press(&mut screen, KeyCode::Enter),
        Some(set_role("slow", &["anthropic/claude-opus-5-5:medium", HAIKU]))
    );
    // A model that does not think is offered no levels.
    let mut screen = on_role("slow");
    press(&mut screen, KeyCode::Enter);
    typed(&mut screen, "haiku");
    assert_eq!(
        press(&mut screen, KeyCode::Enter),
        Some(set_role("slow", &[HAIKU, HAIKU]))
    );
    assert!(screen.strip.is_none());
    // A role that only falls back gets a chain of its own.
    let mut screen = on_role("smol");
    press(&mut screen, KeyCode::Enter);
    typed(&mut screen, "luna");
    assert_eq!(
        press(&mut screen, KeyCode::Enter),
        Some(set_role("smol", &[LUNA]))
    );
}

#[test]
fn thinking_fallbacks_clearing_and_reordering_change_the_chain() {
    let mut screen = on_role("default");
    // `t`: the levels of the role's model, starting on the one it has.
    press(&mut screen, KeyCode::Char('t'));
    let strip = shown(&screen, 140, 20).pop().unwrap();
    assert!(strip.contains("thinking for default:"), "{strip}");
    press(&mut screen, KeyCode::Home);
    assert_eq!(
        press(&mut screen, KeyCode::Enter),
        Some(set_role("default", &[OPUS]))
    );

    // `f` adds a fallback after the others.
    let mut screen = on_role("slow");
    press(&mut screen, KeyCode::Char('f'));
    typed(&mut screen, "opus");
    assert_eq!(
        press(&mut screen, KeyCode::Enter),
        Some(set_role("slow", &[LUNA, HAIKU, OPUS]))
    );
    answered(&mut screen, "slow", &[LUNA, HAIKU, OPUS]);
    press(&mut screen, KeyCode::Esc);

    // On the last fallback: `[` moves it up, but never above the first model.
    press(&mut screen, KeyCode::Down);
    press(&mut screen, KeyCode::Down);
    assert_eq!(
        press(&mut screen, KeyCode::Char('[')),
        Some(set_role("slow", &[LUNA, OPUS, HAIKU]))
    );
    answered(&mut screen, "slow", &[LUNA, OPUS, HAIKU]);
    assert_eq!(press(&mut screen, KeyCode::Char('[')), None);
    // `x` on a fallback removes it; on the role, clears it.
    assert_eq!(
        press(&mut screen, KeyCode::Char('x')),
        Some(set_role("slow", &[LUNA, HAIKU]))
    );
    let mut screen = on_role("slow");
    assert_eq!(
        press(&mut screen, KeyCode::Delete),
        Some(Request::SetRole {
            role: "slow".into(),
            chain: None,
            save: SaveTo::Home,
        })
    );
}

#[test]
fn the_cycle_takes_roles_in_and_out_and_moves_them() {
    let mut screen = on_role("default");
    assert_eq!(
        press(&mut screen, KeyCode::Char('c')),
        Some(Request::Cycle {
            spec: "@default".into(),
            change: FavouriteChange::Add,
        })
    );
    assert_eq!(screen.data.cycle, ["@slow", "@default"]);
    assert_eq!(
        press(&mut screen, KeyCode::Char('[')),
        Some(Request::Cycle {
            spec: "@default".into(),
            change: FavouriteChange::Earlier,
        })
    );
    assert_eq!(screen.data.cycle, ["@default", "@slow"]);
    assert!(shown(&screen, 120, 20)[0].contains("ctrl+p cycle: @default › @slow"));
    press(&mut screen, KeyCode::Char('c'));
    assert_eq!(screen.data.cycle, ["@slow"]);
    // A role outside the cycle has no place in it to move.
    assert_eq!(press(&mut screen, KeyCode::Char(']')), None);
}

#[test]
fn enter_on_a_model_offers_the_roles_and_a_fallback_for_one() {
    let mut screen = screen(Opening::default());
    typed(&mut screen, "luna");
    press(&mut screen, KeyCode::Enter);
    let strip = shown(&screen, 140, 20).pop().unwrap();
    // `slow` has luna already.
    assert!(
        strip.contains("gpt-5.6-luna →") && strip.contains("slow ✔"),
        "{strip}"
    );
    press(&mut screen, KeyCode::End);
    press(&mut screen, KeyCode::Enter);
    let strip = shown(&screen, 140, 20).pop().unwrap();
    assert!(strip.contains("as a fallback for"), "{strip}");
    assert_eq!(
        press(&mut screen, KeyCode::Enter),
        Some(set_role("default", &["anthropic/claude-opus-5-5:high", LUNA]))
    );
}

#[test]
fn new_roles_presets_and_the_save_target() {
    let mut screen = on_role("default");
    press(&mut screen, KeyCode::Char('n'));
    typed(&mut screen, "bad name");
    assert_eq!(press(&mut screen, KeyCode::Enter), None);
    assert!(shown(&screen, 120, 20).pop().unwrap().contains("letters, digits"));
    for _ in 0..5 {
        press(&mut screen, KeyCode::Backspace);
    }
    typed(&mut screen, "_x");
    press(&mut screen, KeyCode::Enter);
    assert!(screen.assign.is_some());
    typed(&mut screen, "haiku");
    assert_eq!(
        press(&mut screen, KeyCode::Enter),
        Some(set_role("bad_x", &[HAIKU]))
    );

    let mut screen = on_role("default");
    press(&mut screen, KeyCode::Char('s'));
    typed(&mut screen, "fast");
    assert_eq!(
        press(&mut screen, KeyCode::Enter),
        Some(Request::SavePreset("fast".into()))
    );
    press(&mut screen, KeyCode::Char('l'));
    assert_eq!(
        press(&mut screen, KeyCode::Enter),
        Some(Request::ApplyPreset("deep".into()))
    );

    // `w`: the project's config, once it is trusted.
    press(&mut screen, KeyCode::Char('w'));
    assert_eq!(screen.title(), "Models · saving to project");
    press(&mut screen, KeyCode::Char('x'));
    assert_eq!(screen.save, SaveTo::Project);
    let mut untrusted = Models::new(
        Data {
            roles: roles(false),
            ..data()
        },
        &Opening::default(),
        &BTreeSet::new(),
        Glyphs::default(),
    );
    press(&mut untrusted, KeyCode::Char('w'));
    let alt_w = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::ALT);
    assert!(matches!(untrusted.on_key(&alt_w), Step::Pending));
    assert_eq!(untrusted.title(), "Models · saving to home");
    assert!(shown(&untrusted, 120, 20)[0].contains("not trusted"));
}

#[test]
fn a_change_says_where_it_went_and_when_the_project_wins() {
    let mut screen = on_role("default");
    press(&mut screen, KeyCode::Char('x'));
    let mut roles = roles(true);
    roles.roles[0] = role("default", &[LUNA], RoleSource::Project, &[LUNA]);
    screen.roles_changed(Ok(roles), Some(LUNA.to_string()));
    let (colour, line) = screen.status.clone().unwrap();
    assert_eq!(colour, Role::Warning);
    assert!(line.contains("the project's config sets default too"), "{line}");
    assert_eq!(screen.data.current.as_deref(), Some(LUNA));
    screen.roles_changed(Err("no".into()), None);
    assert_eq!(screen.status.clone().unwrap(), (Role::Error, "no".to_string()));
}

#[test]
fn a_search_counts_matches_and_steps_over_what_has_none() {
    let mut screen = screen(Opening::default());
    press(&mut screen, KeyCode::Left);
    typed(&mut screen, "haiku");
    let lines = shown(&screen, 140, 20);
    let side = |name: &str| {
        lines
            .iter()
            .find(|line| line.trim_start().starts_with(name))
            .unwrap()
            .clone()
    };
    assert!(side("anthropic").contains(" 1 "), "{lines:#?}");
    assert!(side("openai").contains(" 0 "), "{lines:#?}");
    assert!(lines[1].trim_end().ends_with("1/4"), "{}", lines[1]);
    // Down from All models skips openai, which has nothing.
    press(&mut screen, KeyCode::Down);
    assert_eq!(screen.scope, Scope::Provider("anthropic".into()));
    press(&mut screen, KeyCode::Down);
    assert_eq!(
        screen.scope,
        Scope::Roles,
        "around the end, past what has no match"
    );
    // Nothing matches: the list says so.
    let mut screen = self::screen(Opening::default());
    typed(&mut screen, "zzzz");
    assert!(
        shown(&screen, 140, 20)
            .iter()
            .any(|line| line.contains("no model matches"))
    );
}

#[test]
fn a_provider_without_a_credential_offers_a_key_and_its_catalogue() {
    let mut screen = screen(Opening {
        role: None,
        provider: Some("groq".into()),
    });
    let lines = shown(&screen, 120, 20);
    assert!(lines.iter().any(|line| line.contains("groq has no credential")));
    assert!(
        lines
            .iter()
            .any(|line| line.contains("Set GROQ_API_KEY in the environment"))
    );
    assert!(lines.iter().any(|line| line.contains("Add API key (enter)")));
    assert!(lines.iter().any(|line| line.contains("groq/llama-4")));
    assert!(!lines.iter().any(|line| line.contains("Sign in")));
    assert_eq!(
        press(&mut screen, KeyCode::Enter),
        Some(Request::AddKey("groq".into()))
    );
    // A provider with a sign-in offers that first.
    let mut data = data();
    data.providers[0].usable = false;
    let mut screen = Models::new(
        data,
        &Opening {
            role: None,
            provider: Some("anthropic".into()),
        },
        &BTreeSet::new(),
        Glyphs::default(),
    );
    assert_eq!(
        press(&mut screen, KeyCode::Enter),
        Some(Request::SignIn("anthropic".into()))
    );
}

#[test]
fn providers_are_asked_once_in_the_background_and_answers_keep_the_selection() {
    let mut screen = screen(Opening::default());
    assert_eq!(screen.start_refreshes(), ["anthropic", "openai"]);
    assert_eq!(screen.start_refreshes(), Vec::<String>::new());
    let before = screen.selected_model().unwrap().model.clone();
    let mut fresh = model("anthropic/claude-new", &[], true);
    fresh.context_window = 1_000_000;
    screen.refreshed(
        "anthropic",
        Ok(vec![model(OPUS, &["high"], true), model(HAIKU, &[], true), fresh]),
    );
    assert_eq!(screen.selected_model().unwrap().model, before);
    assert!(
        shown(&screen, 140, 20)
            .iter()
            .any(|line| line.contains("claude-new"))
    );
    screen.refreshed("openai", Err("401".into()));
    press(&mut screen, KeyCode::Left);
    press(&mut screen, KeyCode::End);
    press(&mut screen, KeyCode::Up);
    assert_eq!(screen.scope, Scope::Provider("openai".into()));
    assert!(shown(&screen, 140, 20)[0].contains("401"));

    // One that was asked before this screen opened is not asked again; one the selection
    // rests on is, a moment later.
    let asked: BTreeSet<String> = ["anthropic".to_string()].into();
    let mut screen = Models::new(data(), &Opening::default(), &asked, Glyphs::default());
    press(&mut screen, KeyCode::Left);
    press(&mut screen, KeyCode::Down);
    press(&mut screen, KeyCode::Down);
    assert_eq!(screen.scope, Scope::Provider("openai".into()));
    assert_eq!(screen.tick(Instant::now(), 0), None);
    let later = Instant::now() + Duration::from_secs(1);
    assert_eq!(screen.tick(later, 1).as_deref(), Some("openai"));
    assert_eq!(screen.tick(later, 2), None);
    let lines = shown(&screen, 140, 20);
    let spinner = Glyphs::default().spinner()[2];
    assert!(
        lines
            .iter()
            .any(|line| line.contains(&format!("openai {spinner}")) || line.contains(spinner))
    );
    // F5 asks again.
    screen.refreshed("openai", Ok(vec![model(LUNA, &[], true)]));
    assert_eq!(
        press(&mut screen, KeyCode::F(5)),
        Some(Request::Refresh("openai".into()))
    );
}

#[test]
fn a_refresh_that_drops_rows_before_the_selection_keeps_it() {
    // The rows of a provider listed earlier shrink: the selected row of a later provider moves
    // down the list, and the selection must go with it.
    let mut screen = screen(Opening::default());
    press(&mut screen, KeyCode::End);
    let before = screen.selected_model().unwrap().model.clone();
    assert_eq!(before, "openai/gpt-old");
    screen.refreshed("anthropic", Ok(vec![model(OPUS, &["high"], true)]));
    assert_eq!(screen.selected_model().unwrap().model, before);
    // A provider that grows does not move it either.
    let more = (0..5).map(|n| model(&format!("anthropic/extra-{n}"), &[], true));
    screen.refreshed("anthropic", Ok(more.chain([model(OPUS, &[], true)]).collect()));
    assert_eq!(screen.selected_model().unwrap().model, before);
}

#[test]
fn the_mouse_scrolls_selects_activates_and_picks_chips() {
    let mut screen = screen(Opening::default());
    // A frame places what the mouse finds.
    shown(&screen, 140, 20);
    let at = |row: usize, column: usize, kind: MouseKind| Mouse { kind, row, column };
    // Body rows start on the box's row 3 (border, status, search); the body column after the
    // sidebar.
    let body = 2 + screen.placed.borrow().body + 1;
    let first = screen.selected_model().unwrap().model.clone();
    screen.on_mouse(at(4, body, MouseKind::Press));
    let second = screen.selected_model().unwrap().model.clone();
    assert_ne!(first, second);
    assert!(screen.strip.is_none());
    // A second click on it opens the role strip.
    screen.on_mouse(at(4, body, MouseKind::Press));
    assert!(screen.strip.is_some());
    shown(&screen, 140, 20);
    let (row, chips) = screen.placed.borrow().strip.clone().unwrap();
    let (start, _) = chips[2];
    let step = screen.on_mouse(at(row + 1, start + 2 + 1, MouseKind::Press));
    assert!(matches!(step, Step::Do(Request::SetRole { ref role, .. }) if role == "slow"));
    // The wheel moves the selection; over the sidebar it moves the scope.
    screen.strip = None;
    shown(&screen, 140, 20);
    let before = screen.cursor;
    screen.on_mouse(at(5, body, MouseKind::ScrollDown));
    assert_eq!(screen.cursor, before + 1);
    screen.on_mouse(at(5, 4, MouseKind::ScrollUp));
    assert_eq!(screen.scope, Scope::Recent);
    // A click on a scope selects it; a second goes into its list.
    shown(&screen, 140, 20);
    screen.on_mouse(at(3, 4, MouseKind::Press));
    assert_eq!(screen.scope, Scope::Roles);
    screen.on_mouse(at(3, 4, MouseKind::Press));
    assert!(matches!(
        screen.on_key(&key(KeyCode::Char('c'))),
        Step::Do(Request::Cycle { .. })
    ));
    // Outside the text of the box, nothing.
    assert!(matches!(
        screen.on_mouse(at(0, 0, MouseKind::Press)),
        Step::Pending
    ));
}

#[test]
fn below_eighty_columns_tab_steps_through_the_scopes() {
    let mut screen = screen(Opening::default());
    shown(&screen, 60, 20);
    press(&mut screen, KeyCode::Tab);
    assert_eq!(screen.scope, Scope::Provider("anthropic".into()));
    press(&mut screen, KeyCode::BackTab);
    press(&mut screen, KeyCode::BackTab);
    assert_eq!(screen.scope, Scope::Recent);
    // The hints name the arrows in the set of symbols in use.
    let theme = Theme::dark().with_glyphs(GlyphLevel::Ascii);
    let hints = screen.hints(&theme);
    assert!(hints.iter().any(|hint| hint.key == format!("{}{}", theme.glyph(Glyph::MoreUp), theme.glyph(Glyph::MoreDown))));
}
