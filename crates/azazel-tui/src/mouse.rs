//! Mouse input inside a popup, in the popup's own coordinates.
//!
//! The terminal reports the mouse only while the UI asks it to, which takes the terminal's own
//! selection and scrolling away; the UI asks only while a popup is open. A [`Mouse`] is what
//! reaches the popup: row 0 is the box's top border row, column 0 its left border column.

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

/// What the mouse did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseKind {
    /// The left button went down.
    Press,
    /// It moved with the left button down.
    Drag,
    /// It moved with no button down.
    Move,
    ScrollUp,
    ScrollDown,
}

/// A mouse event over a popup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mouse {
    pub kind: MouseKind,
    /// Within the box: 0 is its top border row.
    pub row: usize,
    /// Within the box: 0 is its left border column.
    pub column: usize,
}

impl Mouse {
    /// `event` in the coordinates of a box placed at `(top, left, rows, width)` (what
    /// [`crate::modal::Modal::placed`] says), or `None` when it is outside the box or is not
    /// one of the kinds a popup uses.
    pub fn within(event: &MouseEvent, placed: (usize, usize, usize, usize)) -> Option<Mouse> {
        let kind = match event.kind {
            MouseEventKind::Down(MouseButton::Left) => MouseKind::Press,
            MouseEventKind::Drag(MouseButton::Left) => MouseKind::Drag,
            MouseEventKind::Moved => MouseKind::Move,
            MouseEventKind::ScrollUp => MouseKind::ScrollUp,
            MouseEventKind::ScrollDown => MouseKind::ScrollDown,
            _ => return None,
        };
        let (top, left, rows, width) = placed;
        let (row, column) = (usize::from(event.row), usize::from(event.column));
        let row = row.checked_sub(top).filter(|row| *row < rows)?;
        let column = column.checked_sub(left).filter(|column| *column < width)?;
        Some(Mouse { kind, row, column })
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyModifiers;

    use super::*;

    fn event(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn events_are_placed_in_the_box_or_dropped() {
        let placed = (5, 10, 4, 20);
        let down = MouseEventKind::Down(MouseButton::Left);
        assert_eq!(
            Mouse::within(&event(down, 10, 5), placed),
            Some(Mouse {
                kind: MouseKind::Press,
                row: 0,
                column: 0
            })
        );
        assert_eq!(
            Mouse::within(&event(MouseEventKind::ScrollDown, 29, 8), placed)
                .map(|m| (m.kind, m.row, m.column)),
            Some((MouseKind::ScrollDown, 3, 19))
        );
        for (column, row) in [(9, 5), (30, 5), (10, 4), (10, 9)] {
            assert_eq!(Mouse::within(&event(down, column, row), placed), None);
        }
        let right = MouseEventKind::Down(MouseButton::Right);
        assert_eq!(Mouse::within(&event(right, 12, 6), placed), None);
    }
}
