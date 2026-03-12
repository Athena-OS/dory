use rand::{prelude::IndexedRandom, RngExt};
use std::{
  collections::VecDeque,
  fs::{File, OpenOptions},
  io::{BufReader, Read, Seek, SeekFrom},
  os::unix::process::ExitStatusExt,
  path::PathBuf,
  process::{Child, Command, Stdio},
  time::{Duration, Instant},
};
use throbber_widgets_tui::{BOX_DRAWING, ThrobberState};

use ansi_to_tui::IntoText;
use fuzzy_matcher::{FuzzyMatcher, skim::SkimMatcherV2};
use ratatui::{
  Frame,
  crossterm::event::{KeyCode, KeyEvent},
  layout::{Alignment, Constraint, Layout, Rect},
  style::{Color, Modifier, Style},
  text::{Line, Span},
  widgets::{
    Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Table, TableState, Wrap,
  },
};

use crate::installer::Signal;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// Manages package selection with efficient fuzzy search and filtering
///
/// This widget provides a two-pane interface for selecting system packages:
/// - Left pane: selected packages
/// - Right pane: available packages with real-time fuzzy search
///
/// Performance optimizations:
/// - Caches fuzzy search results to avoid re-computation
/// - Uses BTreeMap for O(log n) lookups and sorted iteration
/// - Maintains original package order for consistency
#[derive(Debug, Clone)]
pub struct PackageManager {
  // Maps package name -> original index in nixpkgs list
  available: BTreeMap<String, usize>,
  selected: BTreeMap<String, usize>,
  // Original ordering from nixpkgs for restoration
  _original_order: Vec<String>,
  // Cache for filtered results - maps package name to (original_index, fuzzy_score)
  last_filter: Option<String>,
  cached_filtered: BTreeMap<String, (usize, i64)>, // package_name -> (original_index, fuzzy_score)
}

impl PackageManager {
  pub fn new(all_packages: Vec<String>, selected_packages: Vec<String>) -> Self {
    let mut available = BTreeMap::new();
    let mut selected = BTreeMap::new();

    // Build the original order and available map
    for (idx, package) in all_packages.iter().enumerate() {
      available.insert(package.clone(), idx);
    }

    // Move pre-selected packages to selected map
    for package in selected_packages {
      if let Some(idx) = available.remove(&package) {
        selected.insert(package, idx);
      }
    }

    Self {
      available,
      selected,
      _original_order: all_packages,
      last_filter: None,
      cached_filtered: BTreeMap::new(),
    }
  }

  pub fn move_to_selected(&mut self, package: &str) -> bool {
    if let Some(idx) = self.available.remove(package) {
      self.selected.insert(package.to_string(), idx);
      // Update cached filtered map by removing the package
      self.cached_filtered.remove(package);
      true
    } else {
      false
    }
  }

  pub fn move_to_available(&mut self, package: &str) -> bool {
    if let Some(idx) = self.selected.remove(package) {
      self.available.insert(package.to_string(), idx);
      // If we have a cached filter, check if this package matches and add it back
      if let Some(ref filter) = self.last_filter {
        use fuzzy_matcher::{FuzzyMatcher, skim::SkimMatcherV2};
        let matcher = SkimMatcherV2::default();
        if let Some(score) = matcher.fuzzy_match(package, filter) {
          // Add to cached filtered map with both original index and fuzzy score
          self
            .cached_filtered
            .insert(package.to_string(), (idx, score));
        }
      }
      true
    } else {
      false
    }
  }

  pub fn get_available_packages(&self) -> Vec<String> {
    self.available.keys().cloned().collect()
  }

  pub fn get_selected_packages(&self) -> Vec<String> {
    self.selected.keys().cloned().collect()
  }

  /// Filter available packages using fuzzy matching with caching
  ///
  /// Returns packages sorted by relevance score (best matches first)
  /// Caches results to avoid expensive recomputation on repeated searches
  pub fn get_available_filtered(&mut self, filter: &str) -> Vec<String> {
    // Check if we can reuse cached results
    if let Some(ref last_filter) = self.last_filter
      && last_filter == filter {
        return self.get_sorted_by_score_from_cache();
      }

    // Need to recompute fuzzy matches
    use fuzzy_matcher::{FuzzyMatcher, skim::SkimMatcherV2};
    let matcher = SkimMatcherV2::default();

    let mut filtered_map = BTreeMap::new();
    for (package, &original_idx) in &self.available {
      if let Some(score) = matcher.fuzzy_match(package, filter) {
        filtered_map.insert(package.clone(), (original_idx, score));
      }
    }

    // Cache the results for future use
    self.last_filter = Some(filter.to_string());
    self.cached_filtered = filtered_map;

    self.get_sorted_by_score_from_cache()
  }

  /// Get packages from cache sorted by fuzzy match score (best matches first)
  ///
  /// Uses negative score for descending sort (higher scores = better matches)
  fn get_sorted_by_score_from_cache(&self) -> Vec<String> {
    let mut packages_with_score: Vec<_> = self
      .cached_filtered
      .iter()
      .map(|(package, &(_, score))| (package.clone(), score))
      .collect();
    packages_with_score.sort_by_key(|(_, score)| -score); // Negative for descending order
    packages_with_score
      .into_iter()
      .map(|(package, _)| package)
      .collect()
  }

  pub fn contains_available(&self, package: &str) -> bool {
    self.available.contains_key(package)
  }

  pub fn contains_selected(&self, package: &str) -> bool {
    self.selected.contains_key(package)
  }

  /// Get current available packages, respecting any active filter
  ///
  /// Returns filtered results if a search is active, otherwise all available
  /// packages
  pub fn get_current_available(&self) -> Vec<String> {
    if self.last_filter.is_some() {
      self.get_sorted_by_score_from_cache()
    } else {
      self.get_available_packages()
    }
  }
}

pub trait ConfigWidget {
  fn render(&self, f: &mut Frame, area: Rect);
  fn handle_input(&mut self, key: KeyEvent) -> Signal;
  fn interact(&mut self) {}
  fn focus(&mut self);
  fn unfocus(&mut self);
  fn is_focused(&self) -> bool;
  fn get_value(&self) -> Option<Value> {
    None
  }
}

/// Builder pattern for creating complex widget layouts
///
/// Provides a fluent interface for constructing widget containers with:
/// - Custom layouts (horizontal/vertical splits)
/// - Optional borders and titles
/// - Input event callbacks
/// - Child widget management
pub struct WidgetBoxBuilder {
  title: Option<String>,
  layout: Option<Layout>,
  widgets: Vec<Box<dyn ConfigWidget>>,
  input_callback: Option<InputCallbackWidget>,
  render_borders: Option<bool>,
}

impl WidgetBoxBuilder {
  pub fn new() -> Self {
    Self {
      title: None,
      layout: None,
      widgets: vec![],
      input_callback: None,
      render_borders: None,
    }
  }
  pub fn title(mut self, title: impl Into<String>) -> Self {
    self.title = Some(title.into());
    self
  }
  pub fn layout(mut self, layout: Layout) -> Self {
    self.layout = Some(layout);
    self
  }
  pub fn children(mut self, widgets: Vec<Box<dyn ConfigWidget>>) -> Self {
    self.widgets = widgets;
    self
  }
  pub fn input_callback(mut self, callback: InputCallbackWidget) -> Self {
    self.input_callback = Some(callback);
    self
  }
  pub fn render_borders(mut self, render: bool) -> Self {
    self.render_borders = Some(render);
    self
  }
  /// Generate a default horizontal layout that splits space evenly among
  /// widgets
  fn get_default_layout(mut num_widgets: usize) -> Layout {
    if num_widgets == 0 {
      num_widgets = 1; // Prevent division by zero
    }

    // Calculate equal percentage for each widget
    let space_per_widget = 100 / num_widgets;
    let mut constraints = vec![];
    for _ in 0..num_widgets {
      constraints.push(ratatui::layout::Constraint::Percentage(
        space_per_widget as u16,
      ));
    }

    Layout::default()
      .direction(ratatui::layout::Direction::Horizontal)
      .constraints(constraints)
  }
  pub fn build(self) -> WidgetBox {
    let title = self.title.unwrap_or_default();
    let num_widgets = self.widgets.len();
    let layout = self
      .layout
      .unwrap_or_else(|| Self::get_default_layout(num_widgets));
    let render_borders = self.render_borders.unwrap_or(false);
    WidgetBox::new(
      title,
      layout,
      self.widgets,
      self.input_callback,
      render_borders,
    )
  }
}

impl Default for WidgetBoxBuilder {
  fn default() -> Self {
    Self::new()
  }
}

pub type InputCallbackWidget = Box<dyn FnMut(&mut dyn ConfigWidget, KeyEvent) -> Signal>;
pub struct WidgetBox {
  pub focused: bool,
  pub focused_child: Option<usize>,
  pub title: String,
  pub layout: Layout,
  pub widgets: Vec<Box<dyn ConfigWidget>>,
  pub input_callback: Option<InputCallbackWidget>,
  pub render_borders: bool,
}

impl WidgetBox {
  pub fn new(
    title: String,
    layout: Layout,
    widgets: Vec<Box<dyn ConfigWidget>>,
    input_callback: Option<InputCallbackWidget>,
    render_borders: bool,
  ) -> Self {
    Self {
      focused: false,
      focused_child: if widgets.is_empty() { None } else { Some(0) },
      title,
      layout,
      widgets,
      input_callback,
      render_borders,
    }
  }
  /// Alter the children array in-place, without altering the focus state
  pub fn set_children_inplace(&mut self, widgets: Vec<Box<dyn ConfigWidget>>) {
    self.widgets = widgets;
    if self.focused {
      self.focus(); // refreshes focus state for children
    }
  }
  pub fn select_child(&mut self, idx: usize) -> bool {
    if idx < self.widgets.len() {
      if let Some(current_idx) = self.focused_child {
        self.widgets[current_idx].unfocus();
      }
      self.focused_child = Some(idx);
      self.widgets[idx].focus();
      true
    } else {
      false
    }
  }
  pub fn first_child(&mut self) {
    self.select_child(0);
  }
  pub fn last_child(&mut self) {
    self.select_child(self.widgets.len().saturating_sub(1));
  }
  pub fn next_child(&mut self) -> bool {
    let idx = self.focused_child.unwrap_or(0);
    if idx + 1 < self.widgets.len() {
      let next_idx = idx + 1;
      self.widgets[idx].unfocus();
      self.focused_child = Some(next_idx);
      self.widgets[next_idx].focus();

      true
    } else {
      false
    }
  }
  pub fn prev_child(&mut self) -> bool {
    let idx = self.focused_child.unwrap_or(0);
    if idx > 0 {
      let prev_idx = idx - 1;
      self.widgets[idx].unfocus();
      self.focused_child = Some(prev_idx);
      self.widgets[prev_idx].focus();

      true
    } else {
      false
    }
  }
  pub fn selected_child(&self) -> Option<usize> {
    self.focused_child
  }

  pub fn focused_child_mut(&mut self) -> Option<&mut Box<dyn ConfigWidget>> {
    if let Some(idx) = self.focused_child {
      self.widgets.get_mut(idx)
    } else {
      None
    }
  }

  pub fn button_menu(buttons: Vec<Box<dyn ConfigWidget>>) -> Self {
    let num_btns = buttons.len();
    let mut constraints = vec![];
    for _ in 0..num_btns {
      constraints.push(Constraint::Length(1))
    }
    let layout = Layout::default()
      .direction(ratatui::layout::Direction::Vertical)
      .constraints(constraints);
    WidgetBoxBuilder::new()
      .layout(layout)
      .children(buttons)
      .build()
  }
}

impl ConfigWidget for WidgetBox {
  fn handle_input(&mut self, key: KeyEvent) -> Signal {
    self.widgets[self.focused_child.unwrap_or(0)].handle_input(key)
  }

  fn focus(&mut self) {
    self.focused = true;
    let Some(idx) = self.focused_child else {
      self.focused_child = Some(0);
      self.widgets[0].focus();
      return;
    };
    if idx < self.widgets.len() {
      self.widgets[idx].focus();
    } else if !self.widgets.is_empty() {
      self.focused_child = Some(0);
      self.widgets[0].focus();
    }
  }

  fn is_focused(&self) -> bool {
    self.focused
  }

  fn unfocus(&mut self) {
    self.focused = false;
    for widget in self.widgets.iter_mut() {
      widget.unfocus();
    }
  }

  fn render(&self, f: &mut Frame, area: Rect) {
    // If render_borders is enabled, draw a bordered block and reduce the area for
    // inner widgets
    let inner_area = if self.render_borders {
      let block = Block::default()
        .title(self.title.clone())
        .borders(Borders::ALL);
      f.render_widget(block, area);
      Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
      }
    } else {
      area
    };

    let chunks = self.layout.split(inner_area);
    for (i, widget) in self.widgets.iter().enumerate() {
      if let Some(chunk) = chunks.get(i) {
        widget.render(f, *chunk);
      }
    }
  }

  fn get_value(&self) -> Option<Value> {
    let mut map = Map::new();
    for (i, widget) in self.widgets.iter().enumerate() {
      if let Some(value) = widget.get_value() {
        map.insert(format!("widget_{i}"), value);
      }
    }
    if map.is_empty() {
      None
    } else {
      Some(Value::Object(map))
    }
  }
}

pub struct CheckBox {
  pub label: String,
  pub checked: bool,
  pub focused: bool,
}

impl CheckBox {
  pub fn new(label: impl Into<String>, checked: bool) -> Self {
    Self {
      label: label.into(),
      checked,
      focused: false,
    }
  }
  pub fn toggle(&mut self) {
    self.checked = !self.checked;
  }
  pub fn is_checked(&self) -> bool {
    self.checked
  }
}

impl ConfigWidget for CheckBox {
  fn handle_input(&mut self, key: KeyEvent) -> Signal {
    match key.code {
      KeyCode::Char(' ') | KeyCode::Enter => {
        self.toggle();
      }
      _ => {}
    }
    Signal::Wait
  }

  fn interact(&mut self) {
    // Implementation of this method is necessary since it is technically stateful,
    // So we must be able to interact with it through the ConfigWidget interface,
    // so that the widget remains reactive in the case of use with WidgetBox for
    // instance.
    self.toggle();
  }

  fn focus(&mut self) {
    self.focused = true;
  }

  fn is_focused(&self) -> bool {
    self.focused
  }

  fn unfocus(&mut self) {
    self.focused = false;
  }

  fn render(&self, f: &mut Frame, area: Rect) {
    let style = if self.focused {
      Style::default()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
    } else {
      Style::default().fg(Color::White).bg(Color::Reset)
    };

    let checkbox_char = if self.checked { "[x]" } else { "[ ]" };
    let content = Paragraph::new(Span::styled(
      format!("{} {}", checkbox_char, self.label),
      style,
    ))
    .alignment(Alignment::Center)
    .block(Block::default().style(style));

    f.render_widget(content, area);
  }

  fn get_value(&self) -> Option<Value> {
    Some(Value::Bool(self.checked))
  }
}

#[derive(Clone)]
pub struct Button {
  pub label: String,
  pub focused: bool,
}

impl Button {
  pub fn new(label: impl Into<String>) -> Self {
    Self {
      label: label.into(),
      focused: false,
    }
  }
}

impl ConfigWidget for Button {
  fn handle_input(&mut self, _key: KeyEvent) -> Signal {
    Signal::Wait
  }

  fn focus(&mut self) {
    self.focused = true;
  }

  fn is_focused(&self) -> bool {
    self.focused
  }

  fn unfocus(&mut self) {
    self.focused = false;
  }

  fn render(&self, f: &mut Frame, area: Rect) {
    let style = if self.focused {
      Style::default()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
    } else {
      Style::default().fg(Color::White).bg(Color::Reset)
    };

    let content = Paragraph::new(Span::styled(format!(" {} ", self.label), style))
      .alignment(Alignment::Center)
      .block(Block::default().style(style));

    f.render_widget(content, area);
  }

  fn get_value(&self) -> Option<Value> {
    None // Buttons do not produce a value
  }
}

pub struct LineEditor {
  pub focused: bool,
  pub placeholder: Option<String>,
  pub is_secret: bool,
  pub title: String,
  pub value: String,
  pub error: Option<String>,
  pub cursor: usize,
}

impl LineEditor {
  pub fn new(title: impl ToString, placeholder: Option<impl ToString>) -> Self {
    let title = title.to_string();
    let placeholder = placeholder.map(|p| p.to_string());
    Self {
      focused: false,
      placeholder,
      title,
      is_secret: false,
      value: String::new(),
      error: None,
      cursor: 0,
    }
  }
  pub fn secret(mut self, is_secret: bool) -> Self {
    self.is_secret = is_secret;
    self
  }
  fn get_placeholder_line(&self, focused: bool) -> Line<'_> {
    if let Some(placeholder) = &self.placeholder {
      if placeholder.is_empty() {
        if focused {
          let span = Span::styled(
            " ",
            Style::default()
              .fg(Color::Indexed(242)) // Better Gray
              .bg(Color::Gray)
              .add_modifier(Modifier::ITALIC),
          );
          Line::from(span)
        } else {
          let span = Span::styled(
            " ",
            Style::default()
              .fg(Color::Indexed(242))
              .add_modifier(Modifier::ITALIC),
          );
          Line::from(span)
        }
      } else {
        let first_char = placeholder.chars().next().unwrap_or(' ');
        let rest = &placeholder[first_char.len_utf8()..];
        let first_char_span = if focused {
          Span::styled(
            first_char.to_string(),
            Style::default()
              .fg(Color::Indexed(242))
              .bg(Color::Gray)
              .add_modifier(Modifier::ITALIC),
          )
        } else {
          Span::styled(
            first_char.to_string(),
            Style::default()
              .fg(Color::Indexed(242))
              .add_modifier(Modifier::ITALIC),
          )
        };
        let rest_span = Span::styled(
          rest.to_string(),
          Style::default()
            .fg(Color::Indexed(242))
            .add_modifier(Modifier::ITALIC),
        );
        Line::from(vec![first_char_span, rest_span])
      }
    } else {
      let span = Span::styled(
        " ",
        Style::default()
          .fg(Color::Indexed(242))
          .bg(Color::Gray)
          .add_modifier(Modifier::ITALIC),
      );
      Line::from(span)
    }
  }
  fn render_line(&self) -> Line<'_> {
    if !self.focused {
      if self.is_secret {
        let masked = "*".repeat(self.value.chars().count());
        let span = Span::raw(masked);
        return Line::from(span);
      } else if !self.value.is_empty() {
        let span = Span::raw(self.value.clone());
        return Line::from(span);
      } else {
        return Line::from(Span::raw(" "));
      }
    }

    if self.value.is_empty() {
      return self.get_placeholder_line(true);
    }

    let mut left = String::new();
    let mut cursor_char = None;
    let mut right = String::new();

    for (i, c) in self.value.chars().enumerate() {
      if i == self.cursor {
        if self.is_secret {
          cursor_char = Some('*');
        } else {
          cursor_char = Some(c);
        }
      } else if i < self.cursor {
        if self.is_secret {
          left.push('*');
        } else {
          left.push(c);
        }
      } else if self.is_secret {
        right.push('*');
      } else {
        right.push(c);
      }
    }

    if self.focused {
      Line::from(vec![
        Span::raw(left),
        Span::styled(
          cursor_char.map_or(" ".to_string(), |c| c.to_string()),
          Style::default().add_modifier(Modifier::REVERSED),
        ),
        Span::raw(right),
      ])
    } else {
      Line::from(vec![
        Span::raw(left),
        Span::raw(cursor_char.map_or(" ".to_string(), |c| c.to_string())),
        Span::raw(right),
      ])
    }
  }
  fn as_widget(&self) -> Paragraph<'_> {
    Paragraph::new(self.render_line()).block(
      Block::default()
        .title(self.title.clone())
        .borders(Borders::ALL),
    )
  }
  pub fn clear(&mut self) {
    self.value.clear();
    self.cursor = 0;
    self.error = None;
  }
  pub fn set_value(&mut self, value: impl ToString) {
    self.value = value.to_string();
    if self.cursor > self.value.len() {
      self.cursor = self.value.len();
    }
    self.error = None;
  }
  /// Set an error message without clearing the field.
  pub fn set_error(&mut self, msg: impl ToString) {
      self.error = Some(msg.to_string());
  }
  /// Clear any error message.
  pub fn clear_error(&mut self) {
      self.error = None;
  }
  pub fn error(&mut self, msg: impl ToString) {
    self.error = Some(msg.to_string());
    self.value.clear();
    self.cursor = 0;
  }
}

impl ConfigWidget for LineEditor {
  fn handle_input(&mut self, key: KeyEvent) -> Signal {
    match key.code {
      KeyCode::Left => {
        if self.cursor > 0 {
          self.cursor -= 1;
        }
      }
      KeyCode::Right => {
        if self.cursor < self.value.len() {
          self.cursor += 1;
        }
      }
      KeyCode::Backspace => {
        if self.cursor > 0 && !self.value.is_empty() {
          self.value.remove(self.cursor - 1);
          self.cursor -= 1;
        }
      }
      KeyCode::Delete => {
        if self.cursor < self.value.len() && !self.value.is_empty() {
          self.value.remove(self.cursor);
        }
      }
      KeyCode::Char(c) => {
        self.value.insert(self.cursor, c);
        self.cursor += 1;
      }
      KeyCode::Home => {
        self.cursor = 0;
      }
      KeyCode::End => {
        self.cursor = self.value.len();
      }
      _ => {}
    }
    if self.cursor > self.value.len() {
      self.cursor = self.value.len();
    }
    Signal::Wait
  }

  fn render(&self, f: &mut Frame, area: Rect) {
    let chunks = split_vert!(area, 0, [Constraint::Min(3), Constraint::Length(3)]);
    if let Some(err) = &self.error {
      let error_paragraph = Paragraph::new(Span::styled(
        err.clone(),
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
      ))
      .block(Block::default());
      f.render_widget(error_paragraph, chunks[1]);
    }
    let paragraph = self.as_widget();
    f.render_widget(paragraph, chunks[0]);
  }

  fn focus(&mut self) {
    self.focused = true;
    if self.cursor > self.value.len() {
      self.cursor = self.value.len();
    }
  }

  fn is_focused(&self) -> bool {
    self.focused
  }

  fn unfocus(&mut self) {
    self.focused = false;
  }

  fn get_value(&self) -> Option<Value> {
    Some(Value::String(self.value.clone()))
  }
}

pub struct StrListItem {
  pub idx: usize,
}

pub struct StrList {
  pub focused: bool,
  pub title: String,
  pub items: Vec<String>,
  pub filtered_items: Vec<StrListItem>, // after filtering
  pub filter: Option<String>,
  pub selected_idx: usize,
  pub committed_idx: Option<usize>,
  pub committed: Option<String>,
}

impl StrList {
  pub fn new(title: impl Into<String>, items: Vec<String>) -> Self {
    let filtered_items = items
      .iter()
      .cloned()
      .enumerate()
      .map(|(i, _)| StrListItem { idx: i })
      .collect();
    Self {
      focused: false,
      title: title.into(),
      filtered_items,
      items,
      filter: None,
      selected_idx: 0,
      committed_idx: None,
      committed: None,
    }
  }
  pub fn selected_item(&self) -> Option<&String> {
    let item_idx = self.filtered_items.get(self.selected_idx)?;
    self.items.get(item_idx.idx)
  }
  pub fn next_item(&mut self) -> bool {
    if self.selected_idx + 1 < self.filtered_items.len() {
      self.selected_idx += 1;
      true
    } else {
      false
    }
  }
  pub fn previous_item(&mut self) -> bool {
    if self.selected_idx > 0 {
      self.selected_idx -= 1;
      true
    } else {
      false
    }
  }
  pub fn first_item(&mut self) {
    self.selected_idx = 0;
  }
  pub fn last_item(&mut self) {
    self.selected_idx = self.items.len().saturating_sub(1);
  }
  pub fn len(&self) -> usize {
    self.items.len()
  }
  pub fn is_empty(&self) -> bool {
    self.items.is_empty()
  }
  pub fn sort(&mut self) {
    self.items.sort();
    self.set_filter(self.filter.clone());
  }
  pub fn sort_by<F>(&mut self, mut compare: F)
  where
    F: FnMut(&String, &String) -> std::cmp::Ordering,
  {
    self.items.sort_by(|a, b| compare(a, b));
    self.set_filter(self.filter.clone());
  }
  pub fn set_items(&mut self, items: Vec<String>) {
    self.items = items;
    if self.selected_idx >= self.items.len() {
      self.selected_idx = self.items.len().saturating_sub(1);
    }
    self.set_filter(self.filter.clone());
  }
  pub fn set_filter(&mut self, filter: Option<impl Into<String>>) {
    let matcher = SkimMatcherV2::default();
    if let Some(f) = filter {
      let f = f.into();
      self.filter = Some(f.clone());
      let mut results: Vec<_> = self
        .items
        .iter()
        .enumerate()
        .filter_map(|(i, item)| matcher.fuzzy_match(item, &f).map(|score| (i, score)))
        .collect();
      results.sort_unstable_by(|a, b| b.1.cmp(&a.1));
      self.filtered_items = results
        .into_iter()
        .map(|(i, _)| StrListItem { idx: i })
        .collect();
    } else {
      self.filter = None;
      self.filtered_items = self
        .items
        .iter()
        .cloned()
        .enumerate()
        .map(|(i, _)| StrListItem { idx: i })
        .collect();
    }
    self.selected_idx = 0;
  }
  pub fn push_item(&mut self, item: impl Into<String>) {
    self.items.push(item.into());
  }
  pub fn push_unique(&mut self, item: impl Into<String>) -> bool {
    let item = item.into();
    if !self.items.contains(&item) {
      self.push_item(item);
      true
    } else {
      false
    }
  }
  pub fn push_sort_unique(&mut self, item: impl Into<String>) -> bool {
    let added = self.push_unique(item);
    if added {
      self.sort();
    }
    added
  }
  pub fn push_sort(&mut self, item: impl Into<String>) {
    self.push_item(item);
    self.sort();
  }
  pub fn add_item(&mut self, item: impl Into<String>) {
    self.push_item(item);
    self.set_filter(self.filter.clone());
  }
  pub fn remove_item(&mut self, idx: usize) -> Option<String> {
    let idx = self.filtered_items.get(idx).map(|sli| sli.idx)?;
    if idx < self.items.len() {
      let item = self.items.remove(idx);
      self.set_filter(self.filter.clone());
      if self.selected_idx >= self.filtered_items.len() && !self.filtered_items.is_empty() {
        self.selected_idx = self.filtered_items.len() - 1;
      }
      Some(item)
    } else {
      None
    }
  }
  pub fn remove_selected(&mut self) -> Option<String> {
    self.remove_item(self.selected_idx)
  }
}

/// Optimized list widget that works with pre-sorted data and avoids expensive
/// operations
pub struct OptimizedStrList {
  pub focused: bool,
  pub title: String,
  pub items: Vec<String>,
  pub filter: Option<String>,
  pub selected_idx: usize,
}

impl OptimizedStrList {
  pub fn new(title: impl Into<String>, items: Vec<String>) -> Self {
    Self {
      focused: false,
      title: title.into(),
      items,
      filter: None,
      selected_idx: 0,
    }
  }

  pub fn set_items(&mut self, items: Vec<String>) {
    self.items = items;
    if self.selected_idx >= self.items.len() {
      self.selected_idx = self.items.len().saturating_sub(1);
    }
  }

  pub fn selected_item(&self) -> Option<&String> {
    self.items.get(self.selected_idx)
  }

  pub fn next_item(&mut self) -> bool {
    if self.selected_idx + 1 < self.items.len() {
      self.selected_idx += 1;
      true
    } else {
      false
    }
  }

  pub fn previous_item(&mut self) -> bool {
    if self.selected_idx > 0 {
      self.selected_idx -= 1;
      true
    } else {
      false
    }
  }

  pub fn len(&self) -> usize {
    self.items.len()
  }

  pub fn is_empty(&self) -> bool {
    self.items.is_empty()
  }

  pub fn focus(&mut self) {
    self.focused = true;
  }

  pub fn unfocus(&mut self) {
    self.focused = false;
  }

  pub fn is_focused(&self) -> bool {
    self.focused
  }
}

impl ConfigWidget for OptimizedStrList {
  fn render(&self, f: &mut ratatui::Frame, area: ratatui::prelude::Rect) {
    use ratatui::{
      prelude::*,
      widgets::{Block, Borders, List, ListItem, ListState},
    };

    let items: Vec<ListItem> = self
      .items
      .iter()
      .map(|item| ListItem::new(item.as_str()))
      .collect();

    let border_color = if self.focused {
      Color::Yellow
    } else {
      Color::Indexed(242)
    };

    let list = List::new(items)
      .block(
        Block::default()
          .title(self.title.as_str())
          .borders(Borders::ALL)
          .border_style(Style::default().fg(border_color)),
      )
      .highlight_style(Style::default().bg(Color::Blue).fg(Color::White));

    let mut state = ListState::default();
    state.select(Some(self.selected_idx));

    f.render_stateful_widget(list, area, &mut state);
  }

  fn handle_input(&mut self, _key: ratatui::crossterm::event::KeyEvent) -> super::Signal {
    super::Signal::Wait
  }

  fn focus(&mut self) {
    self.focused = true;
  }

  fn unfocus(&mut self) {
    self.focused = false;
  }

  fn is_focused(&self) -> bool {
    self.focused
  }
}

impl ConfigWidget for StrList {
  fn handle_input(&mut self, key: KeyEvent) -> Signal {
    match key.code {
      KeyCode::Up | KeyCode::Char('k') => {
        if self.selected_idx > 0 {
          self.selected_idx -= 1;
        }
      }
      KeyCode::Down | KeyCode::Char('j') => {
        if self.selected_idx + 1 < self.items.len() {
          self.selected_idx += 1;
        }
      }
      KeyCode::Enter => {
        self.committed = Some(self.items[self.selected_idx].clone());
        self.committed_idx = Some(self.selected_idx);
      }
      _ => {}
    }
    Signal::Wait
  }
  fn render(&self, f: &mut Frame, area: Rect) {
    let items: Vec<ListItem> = self
      .filtered_items
      .iter()
      .enumerate()
      .map(|(i, item)| {
        let prefix = if Some(i) == self.committed_idx {
          "> "
        } else {
          "  "
        };
        let idx = item.idx;
        let item = &self.items[idx];
        ListItem::new(Span::raw(format!("{prefix}{item}")))
      })
      .collect();

    let mut state = ListState::default();
    state.select(Some(self.selected_idx));

    let list = if self.focused {
      List::new(items)
        .block(
          Block::default()
            .title(self.title.clone())
            .borders(Borders::ALL),
        )
        .highlight_style(
          Style::default()
            .bg(Color::Cyan)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD),
        )
    } else {
      List::new(items)
        .block(
          Block::default()
            .title(self.title.clone())
            .borders(Borders::ALL),
        )
        .highlight_style(Style::default())
    };

    f.render_stateful_widget(list, area, &mut state);
  }
  fn focus(&mut self) {
    self.focused = true;
  }
  fn unfocus(&mut self) {
    self.focused = false;
  }
  fn is_focused(&self) -> bool {
    self.focused
  }
}

pub struct InfoBox<'a> {
  pub title: String,
  pub content: Vec<Line<'a>>,
  pub highlighted: bool,
}

impl<'a> InfoBox<'a> {
  pub fn new(title: impl Into<String>, content: Vec<Line<'a>>) -> Self {
    Self {
      title: title.into(),
      content,
      highlighted: false,
    }
  }
  pub fn highlighted(&mut self, highlighted: bool) {
    self.highlighted = highlighted;
  }
}

impl<'a> ConfigWidget for InfoBox<'a> {
  fn handle_input(&mut self, _key: KeyEvent) -> Signal {
    Signal::Wait
  }
  fn render(&self, f: &mut Frame, area: Rect) {
    let block = if self.highlighted {
      Block::default()
        .title(self.title.clone())
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
    } else {
      Block::default()
        .title(self.title.clone())
        .borders(Borders::ALL)
    };
    let paragraph = Paragraph::new(self.content.clone())
      .block(block)
      .wrap(ratatui::widgets::Wrap { trim: false });
    f.render_widget(paragraph, area);
  }
  fn focus(&mut self) {
    // InfoBox does not need focus
  }
  fn unfocus(&mut self) {
    // InfoBox does not need focus
  }
  fn is_focused(&self) -> bool {
    false
  }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StepStatus {
  Inactive,
  Running,
  Completed,
  Failed,
}

pub struct InstallSteps<'a> {
  pub title: String,
  pub commands: VecDeque<(Line<'a>, VecDeque<Command>)>,
  pub steps: Vec<(Line<'a>, StepStatus)>,
  pub num_steps: usize,
  pub current_step_index: usize,
  pub throbber_state: ThrobberState,
  pub running: bool,
  pub error: bool,
  pub log_path: Option<PathBuf>,
  current_step_commands: Option<VecDeque<Command>>,
  current_command: Option<Child>,
}

impl<'a> InstallSteps<'a> {
  pub fn new(
    title: impl Into<String>,
    commands: impl IntoIterator<Item = (Line<'a>, VecDeque<Command>)>,
  ) -> Self {
    let commands = commands.into_iter().collect::<VecDeque<_>>();
    let steps = commands
      .iter()
      .map(|(line, _)| (line.clone(), StepStatus::Inactive))
      .collect();
    let num_steps = commands.len();

    Self {
      title: title.into(),
      commands,
      steps,
      num_steps,
      current_step_index: 0,
      throbber_state: ThrobberState::default(),
      running: false,
      error: false,
      log_path: None,
      current_step_commands: None,
      current_command: None,
    }
  }

  pub fn progress(&self) -> f64 {
    if self.num_steps == 0 {
      1.0
    } else {
      let num_completed = self
        .steps
        .iter()
        .filter(|step| step.1 == StepStatus::Completed)
        .count();

      num_completed as f64 / self.num_steps as f64
    }
  }

  pub fn start_next_step(&mut self) -> anyhow::Result<()> {
    // If we have a current step still running, don't start a new one
    if self.current_step_commands.is_some() {
      return Ok(());
    }

    // Get the next step
    if let Some((_line, commands)) = self.commands.pop_front() {
      // Update step status
      if self.current_step_index < self.steps.len() {
        self.steps[self.current_step_index].1 = StepStatus::Running;
      }

      // Store the commands for this step
      self.current_step_commands = Some(commands);
    }
    Ok(())
  }

  pub fn start_next_command(&mut self) -> anyhow::Result<()> {
    // Get the next command from the current step
    if let Some(commands) = self.current_step_commands.as_mut()
      && let Some(mut cmd) = commands.pop_front() {
        // Redirect all output to /dev/null
        let null = std::fs::File::create("/dev/null")?;
        cmd
          .stdout(Stdio::from(null.try_clone()?))
          .stderr(Stdio::from(null))
          .stdin(Stdio::null());

        let child = cmd.spawn()?;
        self.current_command = Some(child);
      }
    Ok(())
  }

  pub fn tick(&mut self) -> anyhow::Result<()> {
    // If nothing is currently running, try to start the next step (even after failures)
    if !self.running {
      self.start_next_step()?;
      if self.current_step_commands.is_some() {
        self.running = true;
      } else {
        // no more steps
        return Ok(());
      }
    }
  
    // keep the spinner alive
    self.throbber_state.calc_next();
  
    // If no command is running but the step has commands, start the next command
    if self.current_command.is_none() && self.current_step_commands.is_some() {
      self.start_next_command()?;
    }
  
    // Poll the running command, if any
    if let Some(child) = &mut self.current_command
      && let Ok(Some(status)) = child.try_wait() {
        // command finished
        self.current_command = None;
      
        // success / failure?
        let failed = match status.code() {
          Some(0) => false,
          Some(_) => true,
          None => {
            #[cfg(unix)]
            { status.signal().is_some() }   // e.g. killed by SIGTERM
            #[cfg(not(unix))]
            { true }
          }
        };
      
        if failed {
          // === write a clear error line to the log so LogBox can display it ===
          if let Some(path) = &self.log_path {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
              .create(true)
              .append(true)
              .open(path)
            {
              let step_no = self.current_step_index + 1;
              let label = self.steps.get(self.current_step_index)
                .map(|(l, _)| l.spans.iter().map(|s| s.content.to_string()).collect::<String>())
                .unwrap_or_else(|| "<unknown step>".to_string());
            
              let code = status.code();
              #[cfg(unix)]
              let sig = status.signal();
              #[cfg(not(unix))]
              let sig: Option<i32> = None;
            
              let _ = writeln!(f);
              let _ = writeln!(f, "-----");
              let _ = writeln!(
                f,
                "✗ Step {step_no} ({label}) failed (exit_code={code:?}, signal={sig:?})"
              );
              let _ = writeln!(f, "-----");
              let _ = f.flush();
            }
          }
          // ===================================================================
        
          // mark the step as failed, drop remaining commands of this step
          if self.current_step_index < self.steps.len() {
            self.steps[self.current_step_index].1 = StepStatus::Failed;
          }
          self.current_step_commands = None;
        
          // advance to the next step (we'll start it on the next tick)
          self.current_step_index += 1;
          self.running = false;
        
          // sticky error flag for UI, but do NOT halt the pipeline
          self.error = true;
        
          return Ok(());
        }
      
        // success path: if no more commands in this step, close it as Completed
        if let Some(commands) = &self.current_step_commands
          && commands.is_empty() {
            if self.current_step_index < self.steps.len() {
              self.steps[self.current_step_index].1 = StepStatus::Completed;
            }
            self.current_step_commands = None;
            self.current_step_index += 1;
            self.running = false;
          }
          // else: next command of this step will start on the next tick
      }
  
    Ok(())
  }

  pub fn is_complete(&self) -> bool {
    !self.running && !self.error && self.commands.is_empty() && self.current_step_commands.is_none()
  }

  pub fn has_error(&self) -> bool {
    self.error
  }
}

impl<'a> ConfigWidget for InstallSteps<'a> {
  fn handle_input(&mut self, _key: KeyEvent) -> Signal {
    Signal::Wait
  }

  fn render(&self, f: &mut Frame, area: Rect) {
    let mut lines = Vec::new();

    for (step_line, status) in self.steps.iter() {
      let (prefix, style) = match status {
        StepStatus::Inactive => ("  ", Style::default().fg(Color::Indexed(242))),
        StepStatus::Running => {
          let idx = (self.throbber_state.index() % 4) as usize;
          let throbber_symbol = BOX_DRAWING.symbols[idx];
          (throbber_symbol, Style::default().fg(Color::Cyan))
        }
        StepStatus::Completed => (
          "✓ ",
          Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
        ),
        StepStatus::Failed => (
          "✗ ",
          Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
      };

      let mut step_spans = vec![Span::styled(prefix, style)];
      step_spans.extend(step_line.spans.iter().cloned().map(|mut span| {
        if *status == StepStatus::Inactive {
          span.style = span.style.fg(Color::Indexed(242));
        }
        span
      }));

      lines.push(Line::from(step_spans));
    }

    let paragraph = Paragraph::new(lines)
      .block(
        Block::default()
          .title(self.title.clone())
          .borders(Borders::ALL),
      )
      .wrap(ratatui::widgets::Wrap { trim: true });

    f.render_widget(paragraph, area);
  }

  fn focus(&mut self) {
    // InstallSteps does not need focus
  }

  fn unfocus(&mut self) {
    // InstallSteps does not need focus
  }

  fn is_focused(&self) -> bool {
    false
  }
}

#[derive(Debug, Clone)]
pub struct TableRow {
  pub headers: Vec<String>,
  pub fields: Vec<String>,
}

impl TableRow {
  pub fn get_field(&self, header: &str) -> Option<&String> {
    if let Some(idx) = self
      .headers
      .iter()
      .position(|h| h.to_lowercase() == header.to_lowercase())
    {
      self.fields.get(idx)
    } else {
      None
    }
  }
}

#[derive(Clone, Debug)]
pub struct TableWidget {
  pub focused: bool,
  pub selected_row: Option<usize>,
  pub title: String,
  pub num_fields: usize,
  pub headers: Vec<String>,
  pub rows: Vec<Vec<String>>,
  pub widths: Vec<Constraint>,
}

impl TableWidget {
  pub fn new(
    title: impl Into<String>,
    widths: Vec<Constraint>,
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
  ) -> Self {
    let num_fields = headers.len();
    Self {
      focused: false,
      selected_row: None,
      title: title.into(),
      num_fields,
      headers,
      rows,
      widths,
    }
  }
  pub fn set_rows(&mut self, rows: Vec<Vec<String>>) {
    self.rows = rows;
    if let Some(idx) = self.selected_row
      && idx >= self.rows.len() {
        self.selected_row = None;
      }
  }
  pub fn selected_row(&self) -> Option<usize> {
    self.selected_row
  }
  pub fn last_row(&mut self) {
    if !self.rows.is_empty() {
      self.selected_row = Some(self.rows.len() - 1);
    } else {
      self.selected_row = None;
    }
  }
  pub fn first_row(&mut self) {
    if !self.rows.is_empty() {
      self.selected_row = Some(0);
    } else {
      self.selected_row = None;
    }
  }
  pub fn next_row(&mut self) -> bool {
    let Some(idx) = self.selected_row else {
      self.selected_row = Some(0);
      return self.next_row();
    };
    if idx + 1 < self.rows.len() {
      self.selected_row = Some(idx + 1);
      true
    } else {
      false
    }
  }
  pub fn previous_row(&mut self) -> bool {
    let Some(idx) = self.selected_row else {
      self.selected_row = Some(0);
      return self.previous_row();
    };
    if idx > 0 {
      self.selected_row = Some(idx - 1);
      true
    } else {
      false
    }
  }
  pub fn get_selected_row_info(&self) -> Option<TableRow> {
    if let Some(idx) = self.selected_row {
      self.get_row(idx)
    } else {
      None
    }
  }
  pub fn get_row(&self, idx: usize) -> Option<TableRow> {
    if idx < self.rows.len() {
      Some(TableRow {
        headers: self.headers.clone(),
        fields: self.rows[idx].clone(),
      })
    } else {
      None
    }
  }
  pub fn fix_selection(&mut self) {
    if let Some(idx) = self.selected_row {
      if idx >= self.rows.len() {
        self.selected_row = Some(0);
      }
    } else if !self.rows.is_empty() {
      self.selected_row = Some(0);
    } else {
      self.selected_row = None;
    }
  }
  pub fn rows(&self) -> &Vec<Vec<String>> {
    &self.rows
  }
  pub fn len(&self) -> usize {
    self.rows.len()
  }
  pub fn is_empty(&self) -> bool {
    self.rows.is_empty()
  }
}

impl ConfigWidget for TableWidget {
  fn handle_input(&mut self, key: KeyEvent) -> Signal {
    if let Some(_idx) = self.selected_row.as_mut() {
      match key.code {
        ui_up!() => {
          self.next_row();
        }
        ui_down!() => {
          self.previous_row();
        }
        _ => {}
      }
      Signal::Wait
    } else {
      self.selected_row = Some(0);
      self.handle_input(key)
    }
  }

  fn focus(&mut self) {
    self.focused = true;
    if self.selected_row.is_none() {
      self.selected_row = Some(0);
    }
  }
  fn is_focused(&self) -> bool {
    self.focused
  }

  fn unfocus(&mut self) {
    self.focused = false;
  }

  fn render(&self, f: &mut Frame, area: Rect) {
    let header_cells = self.headers.iter().map(|h| {
      Span::styled(
        h.clone(),
        Style::default()
          .fg(Color::Yellow)
          .add_modifier(Modifier::BOLD),
      )
    });
    let header = ratatui::widgets::Row::new(header_cells)
      .style(Style::default().bg(Color::Indexed(242)))
      .height(1)
      .bottom_margin(1);

    let rows = self.rows.iter().map(|item| {
      let cells = item.iter().map(|c| Span::raw(c.clone()));
      ratatui::widgets::Row::new(cells).height(1)
    });

    let mut state = TableState::default();
    if self.selected_row.is_some_and(|idx| idx >= self.rows.len()) {
      state.select(None);
    } else {
      state.select(self.selected_row);
    }

    let hl_style = if self.focused {
      Style::default()
        .bg(Color::Cyan)
        .fg(Color::Black)
        .add_modifier(Modifier::BOLD)
    } else {
      Style::default()
    };

    let table = Table::new(rows, &self.widths)
      .header(header)
      .block(
        Block::default()
          .title(self.title.clone())
          .borders(Borders::ALL),
      )
      .widths(&self.widths)
      .column_spacing(1)
      .row_highlight_style(hl_style)
      .highlight_symbol(">> ");

    f.render_stateful_widget(table, area, &mut state);
  }
}

pub struct HelpModal<'a> {
  pub visible: bool,
  pub title: String,
  pub content: Vec<Line<'a>>,
}

impl<'a> HelpModal<'a> {
  pub fn new(title: impl Into<String>, content: Vec<Line<'a>>) -> Self {
    Self {
      visible: false,
      title: title.into(),
      content,
    }
  }

  pub fn show(&mut self) {
    self.visible = true;
  }

  pub fn hide(&mut self) {
    self.visible = false;
  }

  pub fn toggle(&mut self) {
    self.visible = !self.visible;
  }

  pub fn render(&self, f: &mut Frame, area: Rect) {
    if !self.visible {
      return;
    }

    // Calculate popup size - 80% of screen
    let popup_width = (area.width as f32 * 0.8) as u16;
    let popup_height = (area.height as f32 * 0.8) as u16;
    let x = (area.width.saturating_sub(popup_width)) / 2;
    let y = (area.height.saturating_sub(popup_height)) / 2;

    let popup_area = Rect {
      x: area.x + x,
      y: area.y + y,
      width: popup_width,
      height: popup_height,
    };

    // Clear the popup area to remove background content
    f.render_widget(Clear, popup_area);

    // Render the help content
    let help_paragraph = Paragraph::new(self.content.clone())
      .block(
        Block::default()
          .title(format!("Help: {} (Press ? or ESC to close)", self.title))
          .borders(Borders::ALL)
          .border_style(Style::default().fg(Color::Yellow))
          .style(Style::default().bg(Color::Black)),
      )
      .style(Style::default().bg(Color::Black).fg(Color::White))
      .wrap(ratatui::widgets::Wrap { trim: true });

    f.render_widget(help_paragraph, popup_area);
  }
}

/// Complete package selection interface with dual-pane layout
///
/// Provides a sophisticated package picker with:
/// - Left pane: currently selected packages
/// - Right pane: available packages with fuzzy search
/// - Search bar: real-time filtering
/// - Help modal: keyboard shortcuts and usage instructions
///
/// Navigation:
/// - Tab: switch between panes
/// - /: focus search bar
/// - Enter: add/remove packages
/// - ?: show help
pub struct PackagePicker {
  pub focused: bool,
  pub package_manager: PackageManager,
  pub selected: OptimizedStrList,
  pub available: OptimizedStrList,
  pub search_bar: LineEditor,
  help_modal: HelpModal<'static>,
  pub current_filter: Option<String>,
}

impl PackagePicker {
  pub fn new(
    title_selected: &str,
    title_available: &str,
    selected_pkgs: Vec<String>,
    available_pkgs: Vec<String>,
  ) -> Self {
    let package_manager = PackageManager::new(available_pkgs.clone(), selected_pkgs.clone());

    let available =
      OptimizedStrList::new(title_available, package_manager.get_available_packages());
    let selected =
      OptimizedStrList::new(title_selected, package_manager.get_selected_packages());
    let mut search_bar = LineEditor::new("Search", Some("Enter a package name..."));
    search_bar.focus(); // <-- start with focus on the search bar

    let help_content = crate::styled_block(vec![
      vec![
        (
          Some((
            ratatui::style::Color::Yellow,
            ratatui::style::Modifier::BOLD,
          )),
          "Tab",
        ),
        (None, " - Switch between lists and search"),
      ],
      vec![
        (
          Some((
            ratatui::style::Color::Yellow,
            ratatui::style::Modifier::BOLD,
          )),
          "↑/↓, j/k",
        ),
        (None, " - Navigate package lists"),
      ],
      vec![
        (
          Some((
            ratatui::style::Color::Yellow,
            ratatui::style::Modifier::BOLD,
          )),
          "Enter",
        ),
        (None, " - Add/remove package to/from selection"),
      ],
      vec![
        (
          Some((
            ratatui::style::Color::Yellow,
            ratatui::style::Modifier::BOLD,
          )),
          "/",
        ),
        (None, " - Focus search bar"),
      ],
      vec![
        (
          Some((
            ratatui::style::Color::Yellow,
            ratatui::style::Modifier::BOLD,
          )),
          "?",
        ),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Search bar filters packages in real-time as you type.",
      )],
      vec![(None, "Filter persists when adding/removing packages.")],
    ]);
    let help_modal = HelpModal::new("Package Picker", help_content);

    Self {
      focused: false,
      package_manager,
      selected,
      available,
      search_bar,
      help_modal,
      current_filter: None,
    }
  }

  pub fn get_selected_packages(&self) -> Vec<String> {
    self.package_manager.get_selected_packages()
  }

  pub fn get_available_packages(&self) -> Vec<String> {
    self.package_manager.get_available_packages()
  }

  fn focus_available(&mut self) {
    self.available.focus();
    self.search_bar.unfocus();
    self.selected.unfocus();
  }

  fn focus_selected(&mut self) {
    self.selected.focus();
    self.search_bar.unfocus();
    self.available.unfocus();
  }

  /// Focus the search bar (used when opening the page)
  pub fn focus_search(&mut self) {
    self.search_bar.focus();
    self.available.unfocus();
    self.selected.unfocus();
  }

  /// General focus method — defaults to search bar
  pub fn focus(&mut self) {
    self.focused = true;
    self.focus_search();
  }

  fn update_available_list(&mut self) {
    let items = self.package_manager.get_current_available();
    self.available.set_items(items);
  }

  pub fn set_filter(&mut self, filter: Option<String>) {
    self.current_filter = filter.clone();
    let items = if let Some(filter) = filter {
      self.package_manager.get_available_filtered(&filter)
    } else {
      self.package_manager.get_available_packages()
    };
    self.available.set_items(items);
    self.available.selected_idx = 0; // reset cursor to first match
  }
}

impl ConfigWidget for PackagePicker {
  fn render(&self, f: &mut Frame, area: Rect) {
    let hor_chunks = split_hor!(
      area,
      0,
      [Constraint::Percentage(50), Constraint::Percentage(50),]
    );
    let vert_chunks_left = split_vert!(
      hor_chunks[0],
      0,
      [Constraint::Length(5), Constraint::Min(0),]
    );
    let vert_chunks_right = split_vert!(
      hor_chunks[1],
      0,
      [Constraint::Length(5), Constraint::Min(0),]
    );

    self.selected.render(f, vert_chunks_left[1]);
    self.search_bar.render(f, vert_chunks_right[0]);
    self.available.render(f, vert_chunks_right[1]);
    self.help_modal.render(f, area);
  }

  fn handle_input(&mut self, event: KeyEvent) -> Signal {
    use ratatui::crossterm::event::KeyCode;

    match event.code {
      KeyCode::Char('?') => {
        self.help_modal.toggle();
        return Signal::Wait;
      }
      KeyCode::Left => {
        if self.available.is_focused() {
          // Available -> Selected
          self.focus_selected();
        } else if self.search_bar.is_focused() {
          // Search -> Selected (move left)
          self.focus_selected();
        } else if self.selected.is_focused() {
          // Selected -> Search (wrap left)
          self.search_bar.focus();
          self.available.unfocus();
          self.selected.unfocus();
        }
        return Signal::Wait;
      }
      KeyCode::Right => {
        if self.selected.is_focused() {
          // Selected -> Available
          self.focus_available();
        } else if self.available.is_focused() {
          // Available -> Search (move right)
          self.search_bar.focus();
          self.available.unfocus();
          self.selected.unfocus();
        } else if self.search_bar.is_focused() {
          // Search -> Available (wrap right)
          self.focus_available();
        }
        return Signal::Wait;
      }
      KeyCode::Esc if self.help_modal.visible => {
        self.help_modal.hide();
        return Signal::Wait;
      }
      _ if self.help_modal.visible => {
        return Signal::Wait;
      }
      _ => {}
    }

    if event.code == KeyCode::Char('/') && !self.search_bar.is_focused() {
      self.search_bar.focus();
      self.search_bar.clear();
      self.available.unfocus();
      self.selected.unfocus();
      return Signal::Wait;
    }
    if self.search_bar.is_focused() {
      match event.code {
        KeyCode::Enter | KeyCode::Tab => {
          self.focus_available();
          Signal::Wait
        }
        KeyCode::Down => {
          self.focus_available();
          Signal::Wait
        }
        KeyCode::Esc => {
          self.search_bar.clear();
          self.set_filter(None);
          self.focus_available();
          Signal::Wait
        }
        _ => {
          let signal = self.search_bar.handle_input(event);
          let filter_text = self
            .search_bar
            .get_value()
            .and_then(|v| v.as_str().map(|s| s.to_string()));

          if let Some(filter) = filter_text {
            if !filter.is_empty() {
              self.set_filter(Some(filter));
            } else {
              self.set_filter(None);
            }
          } else {
            self.set_filter(None);
          }
          signal
        }
      }
    } else if self.selected.is_focused() {
      match event.code {
        crate::ui_down!() => {
          self.selected.next_item();
          Signal::Wait
        }
        crate::ui_up!() => {
          if !self.selected.previous_item() {
            self.search_bar.focus();
            self.selected.unfocus();
          }
          Signal::Wait
        }
        KeyCode::Tab => {
          self.focus_available();
          Signal::Wait
        }
        KeyCode::Enter => {
          let selected_idx = self.selected.selected_idx;
          if let Some(pkg) = self.selected.selected_item()
            && self.package_manager.move_to_available(pkg) {
              self
                .selected
                .set_items(self.package_manager.get_selected_packages());
              self.update_available_list();
              self.selected.selected_idx = selected_idx.min(self.selected.len().saturating_sub(1));
            }
          Signal::Wait
        }
        _ => Signal::Wait,
      }
    } else if self.available.is_focused() {
      match event.code {
        ui_down!() => {
          self.available.next_item();
          Signal::Wait
        }
        ui_up!() => {
          if !self.available.previous_item() {
            self.search_bar.focus();
            self.available.unfocus();
          }
          Signal::Wait
        }
        KeyCode::Tab => {
          self.focus_selected();
          Signal::Wait
        }
        KeyCode::Enter => {
          let selected_idx = self.available.selected_idx;
          if let Some(pkg) = self.available.selected_item()
            && self.package_manager.move_to_selected(pkg) {
              self
                .selected
                .set_items(self.package_manager.get_selected_packages());
              self.update_available_list();
              self.available.selected_idx =
                selected_idx.min(self.available.len().saturating_sub(1));
            }
          Signal::Wait
        }
        _ => Signal::Wait,
      }
    } else {
      self.focus_available();
      Signal::Wait
    }
  }

  fn focus(&mut self) {
    self.focused = true;
    self.search_bar.focus();
    self.available.unfocus();
    self.selected.unfocus();
  }

  fn unfocus(&mut self) {
    self.focused = false;
    self.search_bar.unfocus();
    self.available.unfocus();
    self.selected.unfocus();
  }

  fn is_focused(&self) -> bool {
    self.focused
  }

  fn get_value(&self) -> Option<Value> {
    Some(Value::Array(
      self
        .get_selected_packages()
        .into_iter()
        .map(Value::String)
        .collect(),
    ))
  }
}

pub struct ProgressBar {
  message: String,
  progress: u32, // 0-100
}

impl ProgressBar {
  pub fn new(message: impl Into<String>, progress: u32) -> Self {
    Self {
      message: message.into(),
      progress,
    }
  }
  pub fn set_progress(&mut self, progress: u32) {
    self.progress = progress.clamp(0, 100);
  }
  pub fn set_message(&mut self, message: impl Into<String>) {
    self.message = message.into();
  }
}

impl ConfigWidget for ProgressBar {
  fn handle_input(&mut self, _key: KeyEvent) -> Signal {
    Signal::Wait
  }
  fn render(&self, f: &mut Frame, area: Rect) {
    let gauge = Gauge::default()
      .block(
        Block::default()
          .title(self.message.clone())
          .borders(Borders::ALL),
      )
      .gauge_style(
        Style::default()
          .fg(Color::Green)
          .bg(Color::Black)
          .add_modifier(Modifier::BOLD),
      )
      .percent(self.progress as u16);
    f.render_widget(gauge, area);
  }
  fn focus(&mut self) {
    // ProgressBar does not need focus
  }
  fn unfocus(&mut self) {
    // ProgressBar does not need focus
  }
  fn is_focused(&self) -> bool {
    false
  }
}

/// Widget that displays streaming log output from a file
///
/// Features:
/// - Real-time log file monitoring (similar to 'tail -f')
/// - ANSI color code support for highlighted output
/// - Circular buffer to prevent memory growth
/// - Handles log rotation and file truncation
/// - Efficient incremental reading
// Add/ensure these imports exist at the top of the file:
pub struct LogBox<'a> {
  title: String,
  focused: bool,
  pub line_buf: VecDeque<Line<'a>>, // circular buffer
  max_buf_size: usize,              // max lines to keep
  log_file: Option<File>,           // (kept for API compatibility; not used)
  reader: Option<BufReader<File>>,  // file reader we tail from
  file_pos: u64,                    // current read offset
  log_path: Option<PathBuf>,        // path to the log file
  pending_bytes: Vec<u8>,   // bytes that didn't decode to valid UTF-8 yet
  pending_line: String,     // decoded but not yet newline-terminated text
}

impl<'a> LogBox<'a> {
  pub fn new(title: String) -> Self {
    Self {
      title,
      focused: false,
      line_buf: VecDeque::with_capacity(512),
      max_buf_size: 1000,
      log_file: None,
      reader: None,
      file_pos: 0,
      log_path: None,
      pending_bytes: Vec::new(),
      pending_line: String::new(),
    }
  }

  pub fn open_log<P: Into<PathBuf>>(&mut self, path: P) -> anyhow::Result<()> {
    let path = path.into();

    // Open read-only for tailing; don't truncate here.
    // (You can truncate earlier in your setup if desired.)
    let file = OpenOptions::new()
      .read(true)
      .write(true)
      .create(true)
      .truncate(false)
      .open(&path)?;

    let file_pos = file.metadata()?.len(); // start reading at EOF
    let reader = BufReader::new(file.try_clone()?);
    self.log_file = Some(file);
    self.reader = Some(reader);
    self.file_pos = file_pos;
    self.log_path = Some(path);
    Ok(())
  }

  /// Call this every render tick to read any new bytes appended to the file.
  /// - Keeps a pending fragment if a line didn't end with '\n' yet.
  /// - Preserves ANSI colors by feeding raw text to `IntoText`.
  /// - Detects truncation/rotation and resets cleanly.
  pub fn poll_log(&mut self) -> anyhow::Result<()> {
    let Some(path) = &self.log_path else { return Ok(()); };
    if !path.exists() { return Ok(()); }

    // detect truncation/rotation
    let current_size = std::fs::metadata(path)?.len();
    if current_size < self.file_pos {
      self.file_pos = 0;
      self.pending_bytes.clear();
      // keep pending_line; if the file was truncated we’ll rebuild naturally
    }
    if current_size <= self.file_pos { return Ok(()); }

    // read new bytes
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(self.file_pos))?;
    let mut new_bytes = Vec::new();
    f.read_to_end(&mut new_bytes)?;
    if new_bytes.is_empty() { return Ok(()); }
    self.file_pos = current_size;

    // prepend undecoded tail from last time
    let mut buf = Vec::with_capacity(self.pending_bytes.len() + new_bytes.len());
    buf.extend_from_slice(&self.pending_bytes);
    buf.extend_from_slice(&new_bytes);
    self.pending_bytes.clear();

    // decode only valid UTF-8 prefix; keep remainder for next tick
    let valid_len = match std::str::from_utf8(&buf) {
      Ok(_) => buf.len(),
      Err(e) => e.valid_up_to(),
    };
    let decoded = String::from_utf8_lossy(&buf[..valid_len]).into_owned();
    if valid_len < buf.len() {
      self.pending_bytes.extend_from_slice(&buf[valid_len..]);
    }

    // Stream-normalize: handle '\r' (overwrite), finalize on '\n', skip blanks
    for ch in decoded.chars() {
      match ch {
        '\r' => {
          // terminal-style redraw: toss the current work-in-progress line
          self.pending_line.clear();
        }
        '\n' => {
          // finalize a line; strip ANSI, discard blanks
          let raw = std::mem::take(&mut self.pending_line);
          let stripped = strip_ansi_escapes::strip_str(&raw);
          if !stripped.trim().is_empty() {
            if let Ok(text) = stripped.into_text() {
              for line in text.lines {
                self.line_buf.push_back(line);
                if self.line_buf.len() > self.max_buf_size { self.line_buf.pop_front(); }
              }
            } else {
              self.line_buf.push_back(raw.into());
              if self.line_buf.len() > self.max_buf_size { self.line_buf.pop_front(); }
            }
          }
          // else: skip truly blank lines
        }
        _ => {
          // build the current line
          self.pending_line.push(ch);
        }
      }
    }

    Ok(())
  }

  /// Optional: push lines directly into the UI buffer (not the file).
  pub fn write_log(&mut self, log_output: &str) {
    if let Ok(text) = log_output.into_text() {
      for line in text.lines {
        self.line_buf.push_back(line);
        if self.line_buf.len() > self.max_buf_size {
          self.line_buf.pop_front();
        }
      }
    } else {
      self.line_buf.push_back(log_output.to_string().into());
      if self.line_buf.len() > self.max_buf_size {
        self.line_buf.pop_front();
      }
    }
  }
}

impl<'a> ConfigWidget for LogBox<'a> {
  fn handle_input(&mut self, _key: KeyEvent) -> Signal {
    Signal::Wait
  }

  fn render(&self, f: &mut Frame, area: Rect) {
    // Show the last N lines that fit in the box
    let rows = area.height.saturating_sub(2) as usize;
    let start = self.line_buf.len().saturating_sub(rows);
    let visible: Vec<Line> = self.line_buf.iter().skip(start).cloned().collect();

    let paragraph = Paragraph::new(visible)
      .block(Block::default().title(self.title.clone()).borders(Borders::ALL))
      .wrap(Wrap { trim: true });

    f.render_widget(paragraph, area);
  }

  fn focus(&mut self)   { self.focused = true; }
  fn unfocus(&mut self) { self.focused = false; }
  fn is_focused(&self) -> bool { self.focused }
}

#[derive(Clone)]
enum Phase {
    Typing,
    FullIdle(Instant),   // when it reached full length; store since
    Deleting,
}

pub struct FancyTicker {
    messages: Vec<String>,
    // current: usize, // Used for sequential rotation of messages

    target: String,
    buf: String,
    phase: Phase,
    last_step: Instant,
    type_interval: Duration,
    delete_interval: Duration,
    idle_duration: Duration,
    leet_chance: f32,
    swapcase_chance: f32,
    hue: u16,
}

impl FancyTicker {
    pub fn new(messages: Vec<&str>) -> Self {
        let first = messages.first().unwrap_or(&"").to_string();
        Self {
            messages: messages.into_iter().map(|s| s.to_string()).collect(),
            //current: 0, // Used for sequential rotation of messages
            target: first,
            buf: String::new(),
            phase: Phase::Typing,
            last_step: Instant::now(),
            type_interval: Duration::from_millis(55),
            delete_interval: Duration::from_millis(35),
            idle_duration: Duration::from_millis(1800),
            leet_chance: 0.12,
            swapcase_chance: 0.10,
            hue: 0,
        }
    }

    pub fn set_phrase<S: Into<String>>(&mut self, s: S) {
        self.target = s.into();
        self.buf.clear();
        self.phase = Phase::Typing;
        self.last_step = Instant::now();
    }

    /// Advance the animation.
    pub fn tick(&mut self) {
        let now = Instant::now();
        match self.phase {
            Phase::Typing => {
                if now.duration_since(self.last_step) >= self.type_interval {
                    self.last_step = now;
                    let next_idx = self.buf.len();
                    if next_idx < self.target.len() {
                        let next_char = self.target[next_idx..].chars().next().unwrap();
                        self.buf.push(next_char);
                    }
                    if self.buf.len() == self.target.len() {
                        self.phase = Phase::FullIdle(Instant::now());
                    }
                }
            }
            Phase::FullIdle(since) => {
                if now.duration_since(since) >= self.idle_duration {
                    self.phase = Phase::Deleting;
                    self.last_step = now;
                }
            }
            Phase::Deleting => {
                if now.duration_since(self.last_step) >= self.delete_interval {
                    self.last_step = now;
                    if !self.buf.is_empty() {
                        let mut it = self.buf.chars();
                        it.next();
                        self.buf = it.collect();
                    } else {
                        // rotation here ⤵
                        //self.current = (self.current + 1) % self.messages.len();
                        //self.target = self.messages[self.current].clone();
                        // random rotation of messages
                        let mut rng = rand::rng();
                        self.target = self.messages.choose(&mut rng).unwrap().clone();
                        
                        self.phase = Phase::Typing;
                    }
                }
            }
        }
        // keep the colors moving a bit
        self.hue = self.hue.wrapping_add(4);
    }

    /// Build a “glitched” version of the current buffer for display.
    fn stylized(&self) -> String {
        let mut rng = rand::rng();

        let mut out = String::with_capacity(self.buf.len());
        for (i, ch) in self.buf.chars().enumerate() {
            let mut c = ch;

            // swap case sometimes
            if rng.random::<f32>() < self.swapcase_chance {
                if c.is_ascii_lowercase() {
                    c = c.to_ascii_uppercase();
                } else if c.is_ascii_uppercase() {
                    c = c.to_ascii_lowercase();
                }
            }

            // inject leet sometimes, only on letters we map
            if rng.random::<f32>() < self.leet_chance
                && let Some(sub) = leet_map(c) {
                    c = sub;
                }

            // occasionally replace a letter with a transient symbol while typing
            if matches!(self.phase, Phase::Typing) && rng.random::<f32>() < 0.06 {
                let fuzz = ['*', '#', '$', '=', '+', '~', '§', 'ø', '¤', '∆'];
                c = *fuzz.choose(&mut rng).unwrap();
            }

            // add a subtle breathing space glitch
            if i % 5 == 0 && rng.random::<f32>() < 0.02 {
                out.push(' ');
            }

            out.push(c);
        }
        out
    }

    pub fn render(&self, f: &mut Frame, area: Rect) {
        // Create a cycling color (you can also keep it single-color if you prefer)
        let color = Color::Indexed(16 + ((self.hue / 6) % 216) as u8); // safe 256-color ramp

        let text = self.stylized();
        let block = Block::default()
            .title(Line::from(vec![
                Span::styled(" ✦ ", Style::default().fg(color)),
                Span::styled("Vibe-o-Meter", Style::default().fg(Color::Gray)),
            ]))
            .borders(Borders::ALL);

        let para = Paragraph::new(text)
            .style(Style::default().fg(color).add_modifier(Modifier::BOLD))
            .wrap(Wrap { trim: false })
            .block(block);

        f.render_widget(para, area);
    }
}

fn leet_map(c: char) -> Option<char> {
    Some(match c {
        'a' | 'A' => '4',
        'e' | 'E' => '3',
        'i' | 'I' => '1',
        'o' | 'O' => '0',
        's' | 'S' => '5',
        't' | 'T' => '7',
        'g' | 'G' => '9',
        _ => return None,
    })
}