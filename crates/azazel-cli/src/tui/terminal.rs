//! What the UI tells the terminal besides the frame: the title and the progress of the tab, the
//! bell and notifications, inline images and diagrams, and links to open.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use azazel_agent::ToolOutput;
use azazel_ai::Role as MessageRole;
use azazel_core::EntryKind;
use azazel_tui::components::markdown::Fenced;
use azazel_tui::components::pager::fenced_blocks;
use azazel_tui::term::{Caps, Images};
use azazel_tui::{HistoryItem, Role, text};

use super::Tui;
use super::overlay::Action;
use crate::attention::{Activity, Kind};

/// The most room an image in the transcript takes.
const IMAGE_COLS: u16 = 60;
const IMAGE_ROWS: u16 = 20;
/// A diagram may be wider and taller than a picture: its text has to stay readable.
const DIAGRAM_COLS: u16 = 100;
const DIAGRAM_ROWS: u16 = 30;

/// What the terminal can do: recognised from the environment, then asked (cell size, Sixel),
/// then overridden by `tui.images`.
pub(super) async fn capabilities(images: azazel_core::config::ImageMode) -> (Caps, Option<String>) {
    use azazel_core::config::ImageMode;
    let mut caps = Caps::detect(&|name| std::env::var(name).ok());
    // Under tmux a picture needs a protocol chosen by hand, and the queries would be answered
    // by tmux; without pictures there is nothing to ask about.
    let ask = cfg!(feature = "images") && images != ImageMode::Off && !caps.tmux;
    if ask {
        caps = caps.with_replies(&super::probe::ask(super::probe::LIMIT).await);
    }
    let detected = if cfg!(feature = "images") {
        caps.images
    } else {
        Images::None
    };
    let (images, problem) = match Images::from_setting(&images.to_string(), detected) {
        Ok(images) if cfg!(feature = "images") => (images, None),
        Ok(_) => (Images::None, None),
        Err(problem) => (Images::None, Some(problem)),
    };
    caps.images = images;
    (caps, problem)
}

/// `path` as a `file://` address a terminal can open.
pub(super) fn file_url(path: &Path) -> String {
    let path = path.to_string_lossy().replace('\\', "/");
    let mut url = String::from("file://");
    if !path.starts_with('/') {
        // `C:/dir`: the drive is the first segment of the path.
        url.push('/');
    }
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                url.push(char::from(byte))
            }
            other => url.push_str(&format!("%{other:02X}")),
        }
    }
    url
}

/// The links of a Markdown reply, in order: the destinations of `[text](address)` links and
/// addresses written out.
pub(super) fn links_in(reply: &str) -> Vec<String> {
    let mut links: Vec<String> = Vec::new();
    let mut rest = reply;
    while let Some(found) = text::find_link(rest) {
        let link = rest[found.clone()].to_string();
        // `[https://a.b](https://a.b)` is one link.
        if links.last() != Some(&link) {
            links.push(link);
        }
        rest = &rest[found.end..];
    }
    links
}

/// The first row of a failure, for a notification.
fn headline(error: &str) -> String {
    text::truncate(error.lines().next().unwrap_or_default(), 120, "…")
}

impl Tui {
    /// Writes escape sequences that are not part of a frame. Between frames the cursor is
    /// where the renderer left it, and none of these move it.
    pub(super) fn write_out(&mut self, sequences: &str) {
        if sequences.is_empty() {
            return;
        }
        let out = self.renderer.get_mut();
        let _ = out.write_all(sequences.as_bytes());
        let _ = out.flush();
    }

    /// The session's name, else the folder it works in: what the title shows.
    fn title_label(&self) -> String {
        self.active.session.name().unwrap_or_else(|| {
            self.app
                .paths
                .cwd
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| self.app.paths.cwd.display().to_string())
        })
    }

    fn session_activity(&self) -> Activity {
        let asked = self
            .overlay
            .as_ref()
            .is_some_and(|overlay| overlay.action.asked_by_agent());
        if asked {
            Activity::Waiting
        } else if self.busy || self.agents_active() {
            Activity::Working
        } else if self.run_failed {
            Activity::Failed
        } else {
            Activity::Idle
        }
    }

    /// Brings the terminal's title and progress indicator, and the hold on the machine's sleep,
    /// up to date with what the session is doing. Called before every frame.
    pub(super) fn tell_terminal(&mut self) {
        let now = Instant::now();
        let activity = self.session_activity();
        let label = self.title_label();
        let sequences = self.attention.show(activity, &label, now);
        self.write_out(&sequences);
        self.attention.keep_awake(self.busy || self.agents_active(), now);
    }

    /// Calls for the user when they are not looking: see [`crate::attention::Attention::signal`].
    pub(super) fn attend(&mut self, kind: Kind, text: &str, took: Option<Duration>) {
        let sequences = self.attention.signal(kind, text, took);
        self.write_out(&sequences);
    }

    /// A run of the lead ended after `took`.
    pub(super) fn attend_end(&mut self, failure: Option<&str>, took: Duration) {
        let label = self.title_label();
        match failure {
            Some(error) => self.attend(
                Kind::Failed,
                &format!("{label}: the run failed: {}", headline(error)),
                Some(took),
            ),
            None => self.attend(Kind::Done, &format!("{label}: the agent is done"), Some(took)),
        }
    }

    /// Appends items (text rows and blocks) to the transcript.
    pub(super) fn say_items(&mut self, items: Vec<HistoryItem>) {
        if !items.is_empty() {
            self.after_card = false;
            self.transcript.extend(items);
        }
    }

    /// The most cells a diagram may take on this screen.
    pub(super) fn diagram_room(&self) -> (u16, u16) {
        let (columns, rows) = self.renderer.size();
        (
            columns.min(DIAGRAM_COLS),
            rows.saturating_sub(4).min(DIAGRAM_ROWS),
        )
    }

    /// The pictures a tool result carries (an image that was read, a screenshot, a generated
    /// image), shown under its card where the terminal draws images.
    pub(super) fn show_images(&mut self, output: &ToolOutput) {
        #[cfg(feature = "images")]
        {
            use azazel_ai::Block;
            use base64::Engine as _;
            if self.caps.images == Images::None {
                return;
            }
            let (columns, rows) = self.renderer.size();
            let room = (columns.min(IMAGE_COLS), rows.saturating_sub(4).min(IMAGE_ROWS));
            for block in &output.content {
                let Block::Image { data, .. } = block else {
                    continue;
                };
                let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) else {
                    continue;
                };
                if let Some(picture) = azazel_tui::image::encode(&bytes, &self.caps, room.0, room.1) {
                    // The card stays the last thing above the picture; what follows starts apart.
                    self.transcript.push(picture);
                    self.after_card = false;
                }
            }
        }
        #[cfg(not(feature = "images"))]
        let _ = (output, IMAGE_COLS, IMAGE_ROWS);
    }

    /// The replies of the agent on the current branch, newest first.
    fn reply_texts(&self) -> Vec<String> {
        self.active
            .session
            .branch(None)
            .into_iter()
            .rev()
            .filter_map(|entry| match entry.kind {
                EntryKind::Message { message } if message.role == MessageRole::Assistant => {
                    Some(message.text()).filter(|text| !text.trim().is_empty())
                }
                _ => None,
            })
            .collect()
    }

    /// `/open`: the last link of the last reply; `/open <n>`: its n-th link; `/open diagram`:
    /// the last Mermaid diagram, as an SVG file; `/open <path>`: a file.
    pub(super) fn open_command(&mut self, args: &str) {
        let replies = self.reply_texts();
        let links = replies.first().map(|reply| links_in(reply)).unwrap_or_default();
        let target = match args.trim() {
            "" => match links.last() {
                Some(link) => link.clone(),
                None => return self.info("the last reply has no link; /open <path> opens a file"),
            },
            "diagram" => match self.write_diagram(&replies) {
                Ok(file) => file,
                Err(why) => return self.fail(&why),
            },
            other => match other.parse::<usize>() {
                Ok(n) => match n.checked_sub(1).and_then(|at| links.get(at)) {
                    Some(link) => link.clone(),
                    None => {
                        return self.fail(&match links.len() {
                            0 => "the last reply has no link".to_string(),
                            1 => "the last reply has one link".to_string(),
                            count => format!("the last reply has {count} links"),
                        });
                    }
                },
                Err(_) if other.contains("://") => other.to_string(),
                Err(_) => {
                    let file = azazel_core::paths::canonical(&self.app.paths.cwd.join(other));
                    if !file.exists() {
                        return self.fail(&format!("{} does not exist", file.display()));
                    }
                    file.display().to_string()
                }
            },
        };
        crate::login::open_in_browser(&target);
        self.info(&format!("opening {target}"));
    }

    /// Writes the last Mermaid diagram of the conversation to the artifacts folder as SVG.
    fn write_diagram(&self, replies: &[String]) -> Result<String, String> {
        let source = replies
            .iter()
            .find_map(|reply| {
                fenced_blocks(reply)
                    .into_iter()
                    .rev()
                    .find(|(language, _)| language.eq_ignore_ascii_case("mermaid"))
            })
            .map(|(_, code)| code)
            .ok_or("no reply in this conversation has a mermaid diagram")?;
        #[cfg(feature = "diagrams")]
        {
            let svg = crate::diagram::svg(&source)
                .map_err(|why| format!("the diagram could not be drawn: {why}"))?;
            let dir = self.app.paths.artifacts_dir();
            std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
            let stamp = jiff::Timestamp::now().as_millisecond();
            let file = dir.join(format!("diagram-{stamp}.svg"));
            std::fs::write(&file, svg).map_err(|e| format!("cannot write {}: {e}", file.display()))?;
            Ok(file.display().to_string())
        }
        #[cfg(not(feature = "diagrams"))]
        {
            let _ = source;
            Err("this build of azazel draws no diagrams: install it with `--features diagrams`".to_string())
        }
    }

    /// The fullest usage window of the account `model` (the one answering the lead) runs on,
    /// once it is nearly full, for the status line.
    pub(super) fn usage_segment(&self, model: Option<&azazel_ai::ModelRef>) -> Option<String> {
        let provider = &model?.provider;
        let account = self.app.credentials.selected(provider)?;
        let windows = self.app.router.limits().windows(provider, &account);
        let nearly = crate::usage::nearly_full(&windows)?;
        Some(self.theme.paint(Role::Warning, &format!("◔ {nearly}")))
    }
}

/// The hook that draws finished `mermaid` blocks of a reply as images, in at most `room` cells.
pub(super) fn draw_diagrams(caps: &Caps, room: (u16, u16)) -> impl FnMut(&str, &str) -> Option<Fenced> + '_ {
    move |language, source| {
        #[cfg(feature = "diagrams")]
        {
            crate::diagram::fenced(language, source, caps, room.0, room.1)
        }
        #[cfg(not(feature = "diagrams"))]
        {
            let _ = (language, source, caps, room);
            None
        }
    }
}

impl Action {
    /// Whether the overlay is an agent waiting for the user: an approval, a question, a plan.
    pub(super) fn asked_by_agent(&self) -> bool {
        matches!(
            self,
            Action::Ask { .. }
                | Action::AskFree { .. }
                | Action::Approval { .. }
                | Action::DenyFeedback { .. }
                | Action::Plan { .. }
                | Action::PlanFeedback { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_becomes_a_file_address() {
        assert_eq!(
            file_url(Path::new("/home/me/src/main.rs")),
            "file:///home/me/src/main.rs"
        );
        assert_eq!(
            file_url(Path::new("C:\\Users\\me\\my file.rs")),
            "file:///C:/Users/me/my%20file.rs"
        );
        assert_eq!(file_url(Path::new("/tmp/é#1")), "file:///tmp/%C3%A9%231");
    }

    #[test]
    fn the_links_of_a_reply_are_found_in_order_and_once() {
        let reply = "See [the docs](https://example.com/docs) and https://example.com/b.\n\n\
                     Also [https://example.com/c](https://example.com/c), then file:///tmp/x.txt";
        assert_eq!(
            links_in(reply),
            [
                "https://example.com/docs",
                "https://example.com/b",
                "https://example.com/c",
                "file:///tmp/x.txt"
            ]
        );
        assert!(links_in("no links here").is_empty());
    }

    #[test]
    fn only_requests_of_agents_count_as_waiting_for_the_user() {
        assert!(!Action::Mode.asked_by_agent());
        assert!(!Action::Thinking.asked_by_agent());
        let (reply, _answer) = tokio::sync::oneshot::channel();
        assert!(
            Action::Approval {
                reply,
                has_rule: false
            }
            .asked_by_agent()
        );
    }
}
