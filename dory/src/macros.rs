// UI Navigation Macros
// These macros provide consistent keyboard shortcuts across the TUI

#[macro_export]
/// Key that opens the help modal on every page. F1 never collides with text
/// input, so passwords and passphrases can contain '?'.
macro_rules! ui_help {
  () => {
    KeyCode::F(1)
  };
}

#[macro_export]
/// Help key for pages without text fields: F1 or '?' (vi-style).
macro_rules! ui_help_or_qmark {
  () => {
    KeyCode::F(1) | KeyCode::Char('?')
  };
}

#[macro_export]
/// Keys for closing/quitting: Escape
macro_rules! ui_close {
  () => {
    KeyCode::Esc
  };
}

#[macro_export]
/// Keys for closing/quitting: Escape or 'q' (vi-style)
macro_rules! ui_close_vi {
  () => {
    KeyCode::Esc | KeyCode::Char('q')
  };
}

#[macro_export]
/// Keys for going back: Escape or Left arrow
macro_rules! ui_back {
  () => {
    KeyCode::Esc | KeyCode::Left
  };
}

#[macro_export]
/// Keys for going back: Escape, 'q', Left arrow, or 'h' (vi-style)
macro_rules! ui_back_vi {
  () => {
    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Left | KeyCode::Char('h')
  };
}

#[macro_export]
/// Keys for entering/selecting: Enter, Right arrow, or 'l' (vi-style)
macro_rules! ui_enter {
  () => {
    KeyCode::Enter | KeyCode::Right | KeyCode::Char('l')
  };
}

#[macro_export]
/// Keys for moving down: Down arrow or 'j' (vi-style)
macro_rules! ui_down {
  () => {
    KeyCode::Down | KeyCode::Char('j')
  };
}

#[macro_export]
/// Keys for moving up: Up arrow or 'k' (vi-style)
macro_rules! ui_up {
  () => {
    KeyCode::Up | KeyCode::Char('k')
  };
}

#[macro_export]
/// Keys for moving left: Left arrow or 'h' (vi-style)
macro_rules! ui_left {
  () => {
    KeyCode::Left | KeyCode::Char('h')
  };
}
#[macro_export]
/// Keys for moving right: Right arrow or 'l' (vi-style)
macro_rules! ui_right {
  () => {
    KeyCode::Right | KeyCode::Char('l')
  };
}

#[macro_export]
/// Split a screen area vertically with specified constraints
///
/// Creates a vertical layout that divides the given area into rows.
/// Each constraint defines how much space each row should take.
///
/// Example:
/// ```
/// let chunks = split_vert!(area, 1, [Constraint::Length(3), Constraint::Min(0)]);
/// // Creates two rows: first is 3 units tall, second takes remaining space
/// ```
macro_rules! split_vert {
  ($area:expr, $margin:expr, $constraints:expr) => {{
    use ratatui::layout::{Constraint, Direction, Layout};
    Layout::default()
      .direction(Direction::Vertical)
      .margin($margin)
      .constraints($constraints)
      .split($area)
  }};
}

#[macro_export]
/// Split a screen area horizontally with specified constraints
///
/// Creates a horizontal layout that divides the given area into columns.
/// Each constraint defines how much space each column should take.
///
/// Example:
/// ```
/// let chunks = split_hor!(area, 0, [Constraint::Percentage(50), Constraint::Percentage(50)]);
/// // Creates two equal-width columns
/// ```
macro_rules! split_hor {
  ($area:expr, $margin:expr, $constraints:expr) => {{
    use ratatui::layout::{Constraint, Direction, Layout};
    Layout::default()
      .direction(Direction::Horizontal)
      .margin($margin)
      .constraints($constraints)
      .split($area)
  }};
}
