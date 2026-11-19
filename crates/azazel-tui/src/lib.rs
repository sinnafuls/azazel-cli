//! Inline terminal UI: differential renderer, components, editor, key parsing and themes.
//!
//! The transcript lives in the terminal's own scrollback. [`render::Renderer`] appends finished
//! lines above a live region at the bottom of the screen and redraws only the rows of that region
//! that changed. Components never touch the terminal: they turn state into [`render::Line`]s.
//! Popups are the exception to "inline": [`modal::Modal`] paints them over the whole window on
//! the alternate screen.

pub mod components;
pub mod glyphs;
#[cfg(feature = "images")]
pub mod image;
pub mod keys;
pub mod modal;
pub mod mouse;
pub mod render;
pub mod term;
pub mod text;
pub mod theme;

pub use components::composer::Composer;
pub use components::editor::{Atom, Draft, Editor, EditorView};
pub use components::markdown::StreamingMarkdown;
pub use components::overlay::{Hint, Scroll};
pub use components::pager::Pager;
pub use components::roster::{RosterRow, RosterState};
pub use components::select::{SelectItem, SelectList};
pub use components::spinner::Spinner;
pub use components::tabs::Tab;
pub use components::todo::TodoState;
pub use components::tool_card::{Card, CardState};
pub use components::vim::Vim;
pub use glyphs::{Glyph, GlyphLevel, Glyphs};
pub use keys::{
    Escape, KeyCombo, KeyError, Keymap, decode_escape, disable_enhanced_keys, enable_enhanced_keys,
};
pub use modal::Modal;
pub use render::{Floor, Frame, HistoryItem, Line, Renderer};
pub use theme::{Color, ColorDepth, RESET, Role, Theme, ThemeError};
