//! Inline terminal UI: differential renderer, components, editor, key parsing and themes.
//!
//! The transcript lives in the terminal's own scrollback. [`render::Renderer`] appends finished
//! lines above a live region at the bottom of the screen and redraws only the rows of that region
//! that changed. Components never touch the terminal: they turn state into [`render::Line`]s.

pub mod components;
#[cfg(feature = "images")]
pub mod image;
pub mod keys;
pub mod render;
pub mod term;
pub mod text;
pub mod theme;

pub use components::composer::Composer;
pub use components::editor::{Draft, Editor, EditorView};
pub use components::markdown::StreamingMarkdown;
pub use components::pager::Pager;
pub use components::roster::{RosterRow, RosterState};
pub use components::select::{SelectItem, SelectList};
pub use components::spinner::Spinner;
pub use components::todo::TodoState;
pub use components::tool_card::{Card, CardState};
pub use components::vim::Vim;
pub use keys::{KeyCombo, KeyError, Keymap, disable_enhanced_keys, enable_enhanced_keys};
pub use render::{Floor, Frame, HistoryItem, Line, Renderer};
pub use theme::{Color, ColorDepth, RESET, Role, Theme, ThemeError};
