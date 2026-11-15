//! `/issues`, `/prs`, `/pr` and `/review`: GitHub through the `gh` program. The pickers only read.
//! Opening a pull request and posting a review change GitHub, so both ask first in every mode
//! but bypass; the logic is in [`crate::github`].

use std::time::Duration;

use azazel_ai::ModelSpec;
use azazel_core::side::{Side, SideAnswer, SideQuestion};
use azazel_core::skills::SkillRegistry;
use azazel_tools::gh::{self, Item, Kind, State};
use azazel_tui::{Glyph, KeyCombo, SelectItem};
use crossterm::event::{KeyCode, KeyModifiers};
use tokio_util::sync::CancellationToken;

use super::overlay::{Action, Kind as OverlayKind, Overlay};
use super::{Internal, Tui};
use crate::github::{self as logic, Plan};

/// The role that drafts pull requests, and the one that reviews them.
const DRAFT_ROLE: &str = "smol";
const REVIEW_ROLE: &str = "reviewer";
const DRAFT_LIMIT: Duration = Duration::from_secs(60);
const REVIEW_LIMIT: Duration = Duration::from_secs(300);

/// What came of something asked of GitHub or a model beside the UI loop.
pub(super) enum Done {
    List {
        kind: Kind,
        state: State,
        result: Result<Vec<Item>, String>,
    },
    Plan(Result<Box<Plan>, String>),
    Draft {
        plan: Box<Plan>,
        result: Result<String, String>,
    },
    Opened(Result<String, String>),
    Review {
        number: u64,
        result: Result<String, String>,
    },
    Posted {
        number: u64,
        result: Result<(), String>,
    },
}

/// The keys of the pickers: `tab` steps through open, closed and all; `o` opens the row in the
/// browser.
fn picker_keys() -> Vec<(KeyCombo, &'static str)> {
    vec![
        (KeyCombo::new(KeyCode::Tab, KeyModifiers::NONE), "open/closed/all"),
        (KeyCombo::new(KeyCode::Char('o'), KeyModifiers::NONE), "browser"),
    ]
}

impl Tui {
    /// The `gh` program and the directory it works in, or what is missing.
    fn github_here(&mut self, what: &str) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
        let cwd = self.local_cwd();
        let Some(cwd) = cwd else {
            self.here_only(what);
            return None;
        };
        match crate::share::gh() {
            Ok(program) => Some((program, cwd)),
            Err(why) => {
                self.fail(&format!("{what}: {why}"));
                None
            }
        }
    }

    /// `/issues` and `/prs`: lists what is open, `tab` changes which.
    pub(super) fn github_list(&mut self, kind: Kind, state: State) {
        let what = if kind == Kind::Pr { "/prs" } else { "/issues" };
        let Some((program, cwd)) = self.github_here(what) else {
            return;
        };
        self.notice = Some(format!("listing {} {}s…", state.word(), kind.label()));
        let done = self.internal_tx.clone();
        tokio::spawn(async move {
            let result = gh::list(&program, &cwd, kind, state).await;
            let _ = done.send(Internal::Github(Done::List { kind, state, result }));
        });
    }

    fn picker(&self, kind: Kind, state: State, items: Vec<Item>) -> Overlay {
        let now = jiff::Timestamp::now().as_second();
        let separator = self.theme.glyph(Glyph::Separator).to_string();
        let rows: Vec<SelectItem> = items
            .iter()
            .map(|item| {
                let (label, parts) = logic::row(item, now);
                SelectItem::new(label, parts.join(&separator))
            })
            .collect();
        let title = format!("{}s ({})", capital(kind.label()), state.word());
        let mut body = Vec::new();
        if rows.is_empty() {
            body.push(format!(
                "No {} {}s. tab shows the others.",
                state.word(),
                kind.label()
            ));
        }
        Overlay::select(title, body, rows, false, Action::GhList { kind, state, items })
            .with_keys(picker_keys())
            .with_enter("put #<n> in the input")
    }

    /// The listing arrived, or a request of `/pr` or `/review` got an answer.
    pub(super) fn on_github(&mut self, done: Done) {
        self.notice = None;
        match done {
            Done::List { kind, state, result } => match result {
                Ok(items) => {
                    let overlay = self.picker(kind, state, items);
                    // A listing asked for with `tab` replaces the one it was asked from.
                    match &self.overlay {
                        Some(open) if !matches!(open.action, Action::GhList { .. }) => {
                            self.deferred.push_back(overlay)
                        }
                        _ => self.overlay = Some(overlay),
                    }
                }
                Err(why) => self.fail(&why),
            },
            Done::Plan(Err(why)) => self.info(&why),
            Done::Plan(Ok(plan)) => self.draft_pull_request(plan),
            Done::Draft { plan, result } => self.on_pull_request_draft(plan, result),
            Done::Opened(Ok(link)) => self.info(&format!("pull request opened: {link}")),
            Done::Opened(Err(why)) => self.fail(&format!("no pull request was opened: {why}")),
            Done::Review { number, result } => match result {
                Ok(text) => self.on_review(number, text),
                Err(why) => self.fail(&format!("the review of #{number} failed: {why}")),
            },
            Done::Posted {
                number,
                result: Ok(()),
            } => self.info(&format!("review posted as a comment on #{number}")),
            Done::Posted {
                number,
                result: Err(why),
            } => self.fail(&format!("the review of #{number} was not posted: {why}")),
        }
    }

    /// A key of a picker acts on the row it is on.
    pub(super) fn github_hotkey(&mut self, index: usize, key: usize) -> bool {
        let Some(Action::GhList { kind, state, items }) = self.overlay.as_ref().map(|o| &o.action) else {
            return false;
        };
        let (kind, state) = (*kind, *state);
        let url = items.get(index).map(|item| item.url.clone());
        match key {
            0 => self.github_list(kind, state.next()),
            _ => match url.filter(|url| !url.is_empty()) {
                Some(url) => crate::login::open_in_browser(&url),
                None => self.info("that row has no address to open"),
            },
        }
        true
    }

    /// Asks a model on the side, the role first and then whatever the lead runs on.
    fn side_ask(
        &mut self,
        role: &str,
        purpose: &'static str,
        question: String,
        limit: Duration,
    ) -> Option<impl std::future::Future<Output = Result<SideAnswer, String>> + Send + use<>> {
        let local = self.backend.local();
        let side = local.and_then(|local| local.app.registry.services.get::<Side>());
        let lead = local.map(|local| local.active.lead.clone());
        let (side, lead) = (side?, lead?);
        let mut chain = vec![ModelSpec::Role(role.to_string())];
        chain.extend(lead.status().chain);
        let agent = lead.info().clone();
        Some(async move {
            let mut ignore = |_: &str| {};
            let asked = side.ask(
                SideQuestion {
                    agent: &agent,
                    context: Vec::new(),
                    question,
                    chain,
                    purpose,
                    framed: false,
                },
                &mut ignore,
                CancellationToken::new(),
            );
            match tokio::time::timeout(limit, asked).await {
                Ok(answer) => answer,
                Err(_) => Err("the model took too long".to_string()),
            }
        })
    }

    // ------------------------------------------------------------ /pr

    /// `/pr [title]`: looks at the branch, drafts a title and a description, lets the user edit
    /// them, asks, then pushes the branch and opens the pull request.
    pub(super) fn pr_command(&mut self, hint: &str) {
        let Some((program, cwd)) = self.github_here("/pr") else {
            return;
        };
        self.pr_hint = hint.to_string();
        self.notice = Some("looking at the branch…".to_string());
        let done = self.internal_tx.clone();
        tokio::spawn(async move {
            let result = logic::plan(&program, &cwd).await.map(Box::new);
            let _ = done.send(Internal::Github(Done::Plan(result)));
        });
    }

    fn draft_pull_request(&mut self, plan: Box<Plan>) {
        let done = self.internal_tx.clone();
        let question = logic::pr_request(&plan, &self.pr_hint);
        let Some(asked) = self.side_ask(DRAFT_ROLE, "pull-request", question, DRAFT_LIMIT) else {
            let result = Err("no model is available in this session".to_string());
            return self.on_pull_request_draft(plan, result);
        };
        self.info(&format!(
            "drafting a pull request for {} into {}…",
            plan.branch, plan.base
        ));
        tokio::spawn(async move {
            let result = asked.await.map(|answer| answer.text);
            let _ = done.send(Internal::Github(Done::Draft { plan, result }));
        });
    }

    fn on_pull_request_draft(&mut self, plan: Box<Plan>, result: Result<String, String>) {
        let (mut title, mut body) = match result {
            Ok(text) => logic::split_draft(&text),
            Err(why) => {
                self.info(&format!("No description was drafted: {why}. Write it yourself."));
                (String::new(), String::new())
            }
        };
        if !self.pr_hint.trim().is_empty() {
            title = self.pr_hint.trim().to_string();
        }
        if body.is_empty() && !plan.log.is_empty() {
            body = plan.log.clone();
        }
        let editor_key = self
            .keymap
            .keys_for("external_editor")
            .first()
            .map(|key| format!(" · {key} opens your editor"))
            .unwrap_or_default();
        let lines = vec![
            format!(
                "{} into {}: {} commit(s).",
                plan.branch,
                plan.base,
                plan.log.lines().count()
            ),
            format!(
                "The first line is the title, the rest the description. shift+enter adds a line{editor_key}. \
                 An empty text cancels. Nothing is pushed yet."
            ),
        ];
        let mut overlay = Overlay::input("Pull request", lines, Action::PrEdit(plan));
        if let OverlayKind::Input { editor, .. } = &mut overlay.kind {
            editor.set_text(&format!("{title}\n\n{body}"));
        }
        self.deferred.push_back(overlay);
    }

    // ------------------------------------------------------------ /review

    /// `/review [n]`: the reviewer reads a pull request (the one of this branch by default) and
    /// its findings are shown; posting them is asked afterwards.
    pub(super) fn review_command(&mut self, args: &str) {
        let Some((program, cwd)) = self.github_here("/review") else {
            return;
        };
        let wanted = args.trim().trim_start_matches('#');
        let number = if wanted.is_empty() {
            None
        } else {
            match wanted.parse::<u64>() {
                Ok(number) => Some(number),
                Err(_) => return self.fail("/review takes a pull request number: /review 12"),
            }
        };
        let skill = self
            .backend
            .local()
            .and_then(|local| local.app.registry.services.get::<SkillRegistry>())
            .and_then(|skills| skills.get("code-review"))
            .map(|skill| skill.body)
            .unwrap_or_default();
        self.notice = Some("reviewing…".to_string());
        let done = self.internal_tx.clone();
        // The question is built once the pull request is read; the answer comes back as `Review`.
        let material = async move {
            let number = match number {
                Some(number) => number,
                None => logic::current_pull_request(&program, &cwd).await?,
            };
            let (view, diff) = logic::review_material(&program, &cwd, number).await?;
            Ok::<_, String>((number, logic::review_request(&skill, &view, &diff)))
        };
        let reviewer = self.review_side();
        tokio::spawn(async move {
            let (number, question) = match material.await {
                Ok(found) => found,
                Err(why) => {
                    let _ = done.send(Internal::Github(Done::Review {
                        number: 0,
                        result: Err(why),
                    }));
                    return;
                }
            };
            let result = match reviewer {
                Some(ask) => ask(question).await.map(|answer| answer.text),
                None => Err("no model is available in this session".to_string()),
            };
            let _ = done.send(Internal::Github(Done::Review { number, result }));
        });
    }

    /// A function that asks the reviewer a question.
    fn review_side(&mut self) -> Option<ReviewAsk> {
        let local = self.backend.local();
        let side = local.and_then(|local| local.app.registry.services.get::<Side>())?;
        let lead = local.map(|local| local.active.lead.clone())?;
        let mut chain = vec![ModelSpec::Role(REVIEW_ROLE.to_string())];
        chain.extend(lead.status().chain);
        let agent = lead.info().clone();
        Some(Box::new(move |question| {
            Box::pin(async move {
                let mut ignore = |_: &str| {};
                let asked = side.ask(
                    SideQuestion {
                        agent: &agent,
                        context: Vec::new(),
                        question,
                        chain,
                        purpose: "review",
                        framed: false,
                    },
                    &mut ignore,
                    CancellationToken::new(),
                );
                match tokio::time::timeout(REVIEW_LIMIT, asked).await {
                    Ok(answer) => answer,
                    Err(_) => Err("the reviewer took too long".to_string()),
                }
            })
        }))
    }

    /// The findings are in the transcript; asks whether to post them.
    fn on_review(&mut self, number: u64, text: String) {
        let text = text.trim().to_string();
        self.info(&format!("Review of #{number}:\n\n{text}"));
        if text.is_empty() {
            return;
        }
        if !logic::asks_before_posting(self.backend.mode()) {
            return self.post_review(number, text);
        }
        let items = vec![
            SelectItem::new("Don't post", "the findings stay here"),
            SelectItem::new(
                "Post as a review comment",
                "gh pr review --comment, as the account gh is signed in to; never an approval",
            ),
        ];
        self.deferred.push_back(Overlay::select(
            format!("Post this review on #{number}?"),
            vec!["It would be visible to everyone who can see the pull request.".to_string()],
            items,
            false,
            Action::ReviewPost { number, body: text },
        ));
    }

    fn post_review(&mut self, number: u64, body: String) {
        let Some((program, cwd)) = self.github_here("/review") else {
            return;
        };
        self.notice = Some("posting the review…".to_string());
        let done = self.internal_tx.clone();
        tokio::spawn(async move {
            let result = logic::post_review(&program, &cwd, number, &body).await;
            let _ = done.send(Internal::Github(Done::Posted { number, result }));
        });
    }

    fn open_pull_request(&mut self, plan: Box<Plan>, title: String, body: String) {
        let Some((program, _)) = self.github_here("/pr") else {
            return;
        };
        self.notice = Some("pushing the branch and opening the pull request…".to_string());
        let done = self.internal_tx.clone();
        tokio::spawn(async move {
            let result = logic::create_pull_request(&program, &plan, &title, &body).await;
            let _ = done.send(Internal::Github(Done::Opened(result)));
        });
    }

    /// What the overlays of this module do when they are answered.
    pub(super) fn finish_github(&mut self, action: Action, choice: usize, typed: String) {
        match action {
            Action::GhList { items, .. } => {
                if let Some(item) = items.get(choice) {
                    self.editor.insert_str(&format!("#{} ", item.number));
                }
            }
            Action::PrEdit(plan) => {
                let (title, body) = logic::split_draft(&typed);
                if title.is_empty() {
                    return self.info("no pull request was opened: the title is empty");
                }
                if !logic::asks_before_posting(self.backend.mode()) {
                    return self.open_pull_request(plan, title, body);
                }
                let mut lines = vec![
                    format!(
                        "Push {} to origin and open a pull request into {}, as the account gh is signed in to.",
                        plan.branch, plan.base
                    ),
                    String::new(),
                    format!("Title: {title}"),
                    String::new(),
                ];
                lines.extend(body.lines().map(str::to_string));
                let items = vec![
                    SelectItem::new("Cancel", "nothing is pushed"),
                    SelectItem::new(
                        "Push and open the pull request",
                        "visible to everyone who can see the repository",
                    ),
                ];
                self.deferred.push_back(Overlay::select(
                    "Open a pull request?",
                    lines,
                    items,
                    false,
                    Action::PrConfirm { plan, title, body },
                ));
            }
            Action::PrConfirm { plan, title, body } if choice == 1 => {
                self.open_pull_request(plan, title, body)
            }
            Action::PrConfirm { .. } => self.info("no pull request was opened"),
            Action::ReviewPost { number, body } if choice == 1 => self.post_review(number, body),
            Action::ReviewPost { .. } => self.info("the review was not posted"),
            _ => {}
        }
    }
}

type ReviewAsk = Box<
    dyn FnOnce(
            String,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<SideAnswer, String>> + Send>>
        + Send,
>;

fn capital(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}
