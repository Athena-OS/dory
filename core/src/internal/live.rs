//! A tiny line-oriented terminal model for package-manager output.
//!
//! pacman prints one line per download, then redraws each bar by jumping to
//! its line with `ESC[nF` / `ESC[nE` (cursor previous/next line, column 0)
//! and ending the bar with `\r`. Fed the raw pty bytes, this model keeps a
//! window of rows that may still be rewritten (the "live" area) and hands out
//! rows that scrolled past that window as finished log lines.

/// Rows above the cursor that pacman may still come back to. pacman never
/// moves up further than its parallel-download block, which is far smaller.
const WINDOW: usize = 64;

#[derive(Default)]
pub struct LiveScreen {
  rows: Vec<Vec<char>>,
  cursor: usize,
  col: usize,
  /// Rows below this index are finished and have been handed out.
  finalized: usize,
  /// Pending escape sequence bytes (`ESC` already seen).
  esc: Option<String>,
}

impl LiveScreen {
  pub fn new() -> Self {
    Self::default()
  }

  /// Feed raw bytes. Returns the rows that became final (complete lines).
  pub fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
    for ch in String::from_utf8_lossy(bytes).chars() {
      self.feed_char(ch);
    }
    self.take_finished()
  }

  fn feed_char(&mut self, ch: char) {
    if let Some(seq) = self.esc.as_mut() {
      if seq.is_empty() {
        if ch == '[' {
          seq.push(ch);
        } else {
          self.esc = None; // not a CSI sequence; drop it
        }
        return;
      }
      if ch.is_ascii_digit() || ch == ';' || ch == '?' {
        seq.push(ch);
        return;
      }
      let params = seq[1..].to_string();
      self.esc = None;
      let n: usize = params
        .split(';')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(1)
        .max(1);
      match ch {
        // CUU / CUD: up/down, column unchanged
        'A' => self.cursor = self.cursor.saturating_sub(n).max(self.finalized),
        'B' => self.cursor += n,
        // CPL / CNL: previous/next line, column 0 (what pacman uses)
        'F' => {
          self.cursor = self.cursor.saturating_sub(n).max(self.finalized);
          self.col = 0;
        }
        'E' => {
          self.cursor += n;
          self.col = 0;
        }
        // CHA: absolute column (1-based)
        'G' => self.col = n - 1,
        'K' => {
          self.ensure_row();
          self.rows[self.cursor].truncate(self.col);
        }
        _ => {} // colours (SGR) and anything else: ignored
      }
      return;
    }
    match ch {
      '\x1b' => self.esc = Some(String::new()),
      '\r' => self.col = 0,
      '\n' => {
        self.cursor += 1;
        self.col = 0;
      }
      '\x07' | '\x08' => {}
      c if c.is_control() => {}
      c => {
        self.ensure_row();
        let row = &mut self.rows[self.cursor];
        if self.col < row.len() {
          row[self.col] = c;
        } else {
          row.resize(self.col, ' ');
          row.push(c);
        }
        self.col += 1;
      }
    }
  }

  fn ensure_row(&mut self) {
    if self.rows.len() <= self.cursor {
      self.rows.resize(self.cursor + 1, Vec::new());
    }
  }

  fn take_finished(&mut self) -> Vec<String> {
    let keep_from = self.cursor.saturating_sub(WINDOW);
    let mut out = Vec::new();
    while self.finalized < keep_from && self.finalized < self.rows.len() {
      let row: String = self.rows[self.finalized].iter().collect();
      if !row.trim().is_empty() {
        out.push(row.trim_end().to_string());
      }
      self.finalized += 1;
    }
    out
  }

  /// Everything not yet final, as the user would currently see it.
  pub fn live_rows(&self) -> Vec<String> {
    self
      .rows
      .iter()
      .skip(self.finalized)
      .map(|r| r.iter().collect::<String>().trim_end().to_string())
      .collect()
  }

  /// Flush all remaining rows as finished (call at EOF).
  pub fn finish(&mut self) -> Vec<String> {
    self.cursor = self.rows.len() + WINDOW;
    self.take_finished()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn carriage_return_redraws_in_place() {
    let mut s = LiveScreen::new();
    assert!(s.feed(b"pkg  10%\rpkg  50%\rpkg 100%").is_empty());
    assert_eq!(s.live_rows(), vec!["pkg 100%"]);
    assert_eq!(s.finish(), vec!["pkg 100%"]);
  }

  #[test]
  fn cursor_up_rewrites_previous_lines() {
    let mut s = LiveScreen::new();
    s.feed(b"a 10%\r\nb 10%\r\n\x1b[2Aa 90%\x1b[K\r\nb 20%\x1b[K\r\n");
    assert_eq!(s.live_rows(), vec!["a 90%", "b 20%"]);
    assert_eq!(s.finish(), vec!["a 90%", "b 20%"]);
  }

  #[test]
  fn pacman_style_multibar_redraw() {
    // Exactly what pacman does: name lines, then jump back with CPL and
    // draw the bar ending in '\r'; "move_end" uses CNL to the empty row.
    let mut s = LiveScreen::new();
    s.feed(b" athena\n core\n");
    s.feed(b"\x1b[2F athena   120.0 KiB  1.2 MiB/s 00:00 [###] 100%\r");
    s.feed(b"\x1b[1E core      10.0 KiB  0.5 MiB/s 00:01 [#--]  33%\r");
    s.feed(b"\x1b[1E"); // console_cursor_move_end
    s.feed(b" extra\n");
    assert_eq!(
      s.live_rows(),
      vec![
        " athena   120.0 KiB  1.2 MiB/s 00:00 [###] 100%",
        " core      10.0 KiB  0.5 MiB/s 00:01 [#--]  33%",
        " extra",
      ]
    );
    // "is up to date" replaces a line and erases the leftovers
    s.feed(b"\x1b[1F extra is up to date\x1b[K");
    assert_eq!(s.live_rows()[2], " extra is up to date");
  }

  #[test]
  fn rows_past_the_window_become_log_lines() {
    let mut s = LiveScreen::new();
    let mut finished = Vec::new();
    for i in 0..(WINDOW + 5) {
      finished.extend(s.feed(format!("line {i}\r\n").as_bytes()));
    }
    assert_eq!(finished.len(), 5);
    assert_eq!(finished[0], "line 0");
    assert_eq!(s.live_rows().len(), WINDOW);
  }

  #[test]
  fn colours_are_stripped_and_erase_truncates() {
    let mut s = LiveScreen::new();
    s.feed(b"\x1b[1;32mok\x1b[0m long text\r\x1b[Kshort");
    assert_eq!(s.live_rows(), vec!["short"]);
  }
}
