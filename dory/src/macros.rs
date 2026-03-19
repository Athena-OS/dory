/// Create a std::process::Command with optional arguments
///
/// This macro simplifies command creation by:
/// - Automatically importing std::process::Command
/// - Converting all arguments to strings
/// - Supporting both command-only and command-with-args patterns
///
/// Examples:
/// ```
/// let cmd1 = command!("ls");
/// let cmd2 = command!("git", "status", "--porcelain");
/// ```
#[macro_export]
macro_rules! command {
		// Command with arguments
		($cmd:expr, $($arg:expr),* $(,)?) => {{
			use std::process::Command;
			let mut c = Command::new($cmd);
				c.args(&[$($arg.to_string()),*]);
				c
		}};
		// Command without arguments
		($cmd:expr) => {{
			use std::process::Command;
			let c = Command::new($cmd);
				c
		}};
}

#[macro_export]
/// Generate Nix attribute set syntax from Rust expressions
///
/// This macro creates properly formatted Nix attribute sets:
/// - Keys are automatically quoted if needed
/// - Values are inserted as-is (use nixstr() for string literals)
/// - Produces valid Nix syntax ready for inclusion in configs
///
/// Example:
/// ```
/// let attrs = attrset! {
///   "services.nginx.enable" = "true";
///   "networking.hostName" = nixstr("myhost");
/// };
/// // Produces: { services.nginx.enable = true; networking.hostName = "myhost"; }
/// ```
macro_rules! attrset {
	{$($key:tt = $val:expr);+ ;} => {{
		let mut parts = vec![];
		$(
			// Remove quotes from string literals for clean Nix attribute names
			parts.push(format!("{} = {};", stringify!($key).trim_matches('"'), $val));
		)*
		format!("{{ {} }}", parts.join(" "))
	}};
}

#[macro_export]
/// Merge multiple Nix attribute sets into one
///
/// This macro combines multiple attribute sets by:
/// - Extracting the contents from each set (removing outer braces)
/// - Concatenating all attributes
/// - Wrapping the result in new braces
/// - Validating that inputs are properly formatted attribute sets
///
/// Example:
/// ```
/// let set1 = attrset! { "a" = "1"; };
/// let set2 = attrset! { "b" = "2"; };
/// let combined = merge_attrs!(set1, set2);
/// // Produces: { a = 1; b = 2; }
/// ```
macro_rules! merge_attrs {
	($($set:expr),* $(,)?) => {{
		let mut merged = String::new();
		$(
			if !$set.is_empty() {
				// Validate that we have a proper attribute set
				if !$set.starts_with('{') || !$set.ends_with('}') {
					panic!("attrset must be a valid attribute set, got: {:?}", $set);
				}
				// Extract the inner content (without braces)
				let inner = $set
				.strip_prefix('{')
				.and_then(|s| s.strip_suffix('}'))
				.unwrap_or("")
				.trim();
				merged.push_str(inner);
			}
		)*
			// Wrap the merged content in braces
			format!("{{ {merged} }}")
	}};
}

#[macro_export]
/// Generate Nix list syntax from Rust expressions
///
/// Creates properly formatted Nix lists with space-separated elements:
/// - Each item is converted to string representation
/// - Items are joined with spaces (Nix list syntax)
/// - Produces valid Nix syntax ready for use in configurations
///
/// Example:
/// ```
/// let packages = list!["git", "vim", "firefox"];
/// // Produces: [git vim firefox]
/// ```
macro_rules! list {
	($($item:expr),* $(,)?) => {
		{
			let items = vec![$(format!("{}", $item)),*];
			format!("[{}]", items.join(" "))
		}
	};
}

// UI Navigation Macros
// These macros provide consistent keyboard shortcuts across the TUI

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
