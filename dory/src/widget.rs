use rand::{RngExt, prelude::IndexedRandom};
use std::{
  collections::VecDeque,
  fs::{File, OpenOptions},
  io::{Read, Seek, SeekFrom},
  os::unix::{fs::MetadataExt, process::ExitStatusExt},
  path::PathBuf,
  process::{Child, Command, Stdio},
  time::{Duration, Instant},
};
use throbber_widgets_tui::{BOX_DRAWING, ThrobberState};

use fuzzy_matcher::{FuzzyMatcher, skim::SkimMatcherV2};
use ratatui::{
  Frame,
  crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
  layout::{Alignment, Constraint, Layout, Rect},
  style::{Color, Modifier, Style},
  text::{Line, Span},
  widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Table, TableState, Wrap},
};

use crate::installer::Signal;
use serde_json::{Map, Value};
use std::collections::BTreeSet;

/// Package selection model: one immutable, sorted list of names plus index
/// sets. Nothing is cloned when filtering or moving packages between panes.
#[derive(Debug, Clone, Default)]
pub struct PackageManager {
  all: Vec<String>,
  selected: BTreeSet<usize>,
  /// Available (not selected) packages matching `filter`, best match first;
  /// all available packages in order when there is no filter.
  available: Vec<usize>,
  filter: Option<String>,
}

impl PackageManager {
  pub fn new(mut all_packages: Vec<String>, selected_packages: &[String]) -> Self {
    all_packages.sort_unstable();
    all_packages.dedup();
    let selected = selected_packages
      .iter()
      .filter_map(|p| all_packages.binary_search(p).ok())
      .collect();
    let mut pm = Self {
      all: all_packages,
      selected,
      available: Vec::new(),
      filter: None,
    };
    pm.recompute();
    pm
  }

  fn recompute(&mut self) {
    let avail = (0..self.all.len()).filter(|i| !self.selected.contains(i));
    self.available = match &self.filter {
      Some(f) => {
        let matcher = SkimMatcherV2::default();
        let mut scored: Vec<(usize, i64)> = avail
          .filter_map(|i| matcher.fuzzy_match(&self.all[i], f).map(|score| (i, score)))
          .collect();
        scored.sort_unstable_by_key(|&(_, score)| std::cmp::Reverse(score));
        scored.into_iter().map(|(i, _)| i).collect()
      }
      None => avail.collect(),
    };
  }

  pub fn set_filter(&mut self, filter: Option<String>) {
    let filter = filter.filter(|f| !f.is_empty());
    if filter != self.filter {
      self.filter = filter;
      self.recompute();
    }
  }

  /// Indices (into `names()`) of the available packages after filtering.
  pub fn available(&self) -> &[usize] {
    &self.available
  }

  /// Indices of the selected packages, sorted by name.
  pub fn selected(&self) -> impl Iterator<Item = usize> + '_ {
    self.selected.iter().copied()
  }

  pub fn selected_len(&self) -> usize {
    self.selected.len()
  }

  pub fn name(&self, idx: usize) -> &str {
    &self.all[idx]
  }

  pub fn select(&mut self, idx: usize) {
    if self.selected.insert(idx) {
      self.available.retain(|&i| i != idx);
    }
  }

  pub fn deselect(&mut self, idx: usize) {
    if self.selected.remove(&idx) {
      // Keep ordering consistent with the filter rather than inserting ad hoc.
      self.recompute();
    }
  }

  pub fn get_selected_packages(&self) -> Vec<String> {
    self.selected.iter().map(|&i| self.all[i].clone()).collect()
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
  render_borders: Option<bool>,
}

impl WidgetBoxBuilder {
  pub fn new() -> Self {
    Self {
      title: None,
      layout: None,
      widgets: vec![],
      render_borders: None,
    }
  }
  pub fn layout(mut self, layout: Layout) -> Self {
    self.layout = Some(layout);
    self
  }
  pub fn children(mut self, widgets: Vec<Box<dyn ConfigWidget>>) -> Self {
    self.widgets = widgets;
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
    WidgetBox::new(title, layout, self.widgets, render_borders)
  }
}

impl Default for WidgetBoxBuilder {
  fn default() -> Self {
    Self::new()
  }
}

pub struct WidgetBox {
  pub focused: bool,
  pub focused_child: Option<usize>,
  pub title: String,
  pub layout: Layout,
  pub widgets: Vec<Box<dyn ConfigWidget>>,
  pub render_borders: bool,
}

impl WidgetBox {
  pub fn new(
    title: String,
    layout: Layout,
    widgets: Vec<Box<dyn ConfigWidget>>,
    render_borders: bool,
  ) -> Self {
    Self {
      focused: false,
      focused_child: if widgets.is_empty() { None } else { Some(0) },
      title,
      layout,
      widgets,
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
  /// Text buffer as chars so cursor arithmetic is Unicode-safe.
  chars: Vec<char>,
  pub error: Option<String>,
  /// Cursor position in *characters* (0..=chars.len()).
  pub cursor: usize,
}

const PLACEHOLDER_FG: Color = Color::Indexed(242);

impl LineEditor {
  pub fn new(title: impl ToString, placeholder: Option<impl ToString>) -> Self {
    Self {
      focused: false,
      placeholder: placeholder.map(|p| p.to_string()),
      title: title.to_string(),
      is_secret: false,
      chars: Vec::new(),
      error: None,
      cursor: 0,
    }
  }
  pub fn secret(mut self, is_secret: bool) -> Self {
    self.is_secret = is_secret;
    self
  }
  /// Current text.
  pub fn value(&self) -> String {
    self.chars.iter().collect()
  }
  fn placeholder_style(&self, highlight: bool) -> Style {
    let style = Style::default()
      .fg(PLACEHOLDER_FG)
      .add_modifier(Modifier::ITALIC);
    if highlight {
      style.bg(Color::Gray)
    } else {
      style
    }
  }
  fn get_placeholder_line(&self, focused: bool) -> Line<'_> {
    match self.placeholder.as_deref() {
      Some(placeholder) if !placeholder.is_empty() => {
        let mut it = placeholder.chars();
        let first = it.next().unwrap_or(' ').to_string();
        let rest: String = it.collect();
        Line::from(vec![
          Span::styled(first, self.placeholder_style(focused)),
          Span::styled(rest, self.placeholder_style(false)),
        ])
      }
      _ => Line::from(Span::styled(" ", self.placeholder_style(true))),
    }
  }
  fn shown(&self, c: char) -> char {
    if self.is_secret { '*' } else { c }
  }
  fn render_line(&self) -> Line<'_> {
    if !self.focused {
      if self.chars.is_empty() {
        return Line::from(Span::raw(" "));
      }
      return Line::from(Span::raw(
        self
          .chars
          .iter()
          .map(|&c| self.shown(c))
          .collect::<String>(),
      ));
    }
    if self.chars.is_empty() {
      return self.get_placeholder_line(true);
    }

    let cursor = self.cursor.min(self.chars.len());
    let left: String = self.chars[..cursor]
      .iter()
      .map(|&c| self.shown(c))
      .collect();
    let cursor_char = self
      .chars
      .get(cursor)
      .map_or(" ".to_string(), |&c| self.shown(c).to_string());
    let right: String = self
      .chars
      .get(cursor + 1..)
      .unwrap_or(&[])
      .iter()
      .map(|&c| self.shown(c))
      .collect();

    Line::from(vec![
      Span::raw(left),
      Span::styled(
        cursor_char,
        Style::default().add_modifier(Modifier::REVERSED),
      ),
      Span::raw(right),
    ])
  }
  fn as_widget(&self) -> Paragraph<'_> {
    Paragraph::new(self.render_line()).block(
      Block::default()
        .title(self.title.as_str())
        .borders(Borders::ALL),
    )
  }
  pub fn clear(&mut self) {
    self.chars.clear();
    self.cursor = 0;
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
  /// Set an error message and clear the field.
  pub fn error(&mut self, msg: impl ToString) {
    self.error = Some(msg.to_string());
    self.chars.clear();
    self.cursor = 0;
  }
}

impl ConfigWidget for LineEditor {
  fn handle_input(&mut self, key: KeyEvent) -> Signal {
    match key.code {
      KeyCode::Left if self.cursor > 0 => self.cursor -= 1,
      KeyCode::Right if self.cursor < self.chars.len() => self.cursor += 1,
      KeyCode::Backspace if self.cursor > 0 => {
        self.chars.remove(self.cursor - 1);
        self.cursor -= 1;
      }
      KeyCode::Delete if self.cursor < self.chars.len() => {
        self.chars.remove(self.cursor);
      }
      KeyCode::Char(c)
        if !key
          .modifiers
          .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
      {
        self.chars.insert(self.cursor, c);
        self.cursor += 1;
      }
      KeyCode::Home => self.cursor = 0,
      KeyCode::End => self.cursor = self.chars.len(),
      _ => {}
    }
    Signal::Wait
  }

  fn render(&self, f: &mut Frame, area: Rect) {
    let chunks = split_vert!(area, 0, [Constraint::Min(3), Constraint::Length(3)]);
    if let Some(err) = &self.error {
      let error_paragraph = Paragraph::new(Span::styled(
        err.as_str(),
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
      ));
      f.render_widget(error_paragraph, chunks[1]);
    }
    f.render_widget(self.as_widget(), chunks[0]);
  }

  fn focus(&mut self) {
    self.focused = true;
    self.cursor = self.cursor.min(self.chars.len());
  }

  fn is_focused(&self) -> bool {
    self.focused
  }

  fn unfocus(&mut self) {
    self.focused = false;
  }

  fn get_value(&self) -> Option<Value> {
    Some(Value::String(self.value()))
  }
}

pub struct StrList {
  pub focused: bool,
  pub title: String,
  pub items: Vec<String>,
  /// Indices into `items` that pass the current filter, best match first.
  pub filtered: Vec<usize>,
  pub filter: Option<String>,
  /// Cursor position inside `filtered`.
  pub selected_idx: usize,
  pub committed_idx: Option<usize>,
  pub committed: Option<String>,
}

impl StrList {
  pub fn new(title: impl Into<String>, items: Vec<String>) -> Self {
    Self {
      focused: false,
      title: title.into(),
      filtered: (0..items.len()).collect(),
      items,
      filter: None,
      selected_idx: 0,
      committed_idx: None,
      committed: None,
    }
  }
  /// The item under the cursor (respects the active filter).
  pub fn selected_item(&self) -> Option<&String> {
    self.items.get(*self.filtered.get(self.selected_idx)?)
  }
  pub fn next_item(&mut self) -> bool {
    if self.selected_idx + 1 < self.filtered.len() {
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
    self.selected_idx = self.filtered.len().saturating_sub(1);
  }
  pub fn set_filter(&mut self, filter: Option<impl Into<String>>) {
    match filter.map(Into::into).filter(|f| !f.is_empty()) {
      Some(f) => {
        let matcher = SkimMatcherV2::default();
        let mut results: Vec<(usize, i64)> = self
          .items
          .iter()
          .enumerate()
          .filter_map(|(i, item)| matcher.fuzzy_match(item, &f).map(|score| (i, score)))
          .collect();
        results.sort_unstable_by_key(|&(_, score)| std::cmp::Reverse(score));
        self.filtered = results.into_iter().map(|(i, _)| i).collect();
        self.filter = Some(f);
      }
      None => {
        self.filtered = (0..self.items.len()).collect();
        self.filter = None;
      }
    }
    self.selected_idx = 0;
  }
}

impl ConfigWidget for StrList {
  fn handle_input(&mut self, key: KeyEvent) -> Signal {
    match key.code {
      KeyCode::Up | KeyCode::Char('k') => {
        self.previous_item();
      }
      KeyCode::Down | KeyCode::Char('j') => {
        self.next_item();
      }
      KeyCode::Enter => {
        self.committed = self.selected_item().cloned();
        self.committed_idx = Some(self.selected_idx);
      }
      _ => {}
    }
    Signal::Wait
  }
  fn render(&self, f: &mut Frame, area: Rect) {
    let items: Vec<ListItem> = self
      .filtered
      .iter()
      .enumerate()
      .map(|(i, &idx)| {
        let prefix = if Some(i) == self.committed_idx {
          "> "
        } else {
          "  "
        };
        ListItem::new(Span::raw(format!("{prefix}{}", self.items[idx])))
      })
      .collect();

    let mut state = ListState::default();
    state.select(Some(self.selected_idx));

    let highlight = if self.focused {
      Style::default()
        .bg(Color::Cyan)
        .fg(Color::Black)
        .add_modifier(Modifier::BOLD)
    } else {
      Style::default()
    };
    let list = List::new(items)
      .block(
        Block::default()
          .title(self.title.as_str())
          .borders(Borders::ALL),
      )
      .highlight_style(highlight);

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
    Self {
      title: title.into(),
      commands,
      steps,
      current_step_index: 0,
      throbber_state: ThrobberState::default(),
      running: false,
      error: false,
      log_path: None,
      current_step_commands: None,
      current_command: None,
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
      && let Some(mut cmd) = commands.pop_front()
    {
      // stdout/stderr are configured by whoever built the command (the
      // installer redirects them to the log file); never read from the TTY.
      cmd.stdin(Stdio::null());

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
      && let Ok(Some(status)) = child.try_wait()
    {
      // command finished
      self.current_command = None;

      // success / failure?
      let failed = match status.code() {
        Some(0) => false,
        Some(_) => true,
        None => {
          #[cfg(unix)]
          {
            status.signal().is_some()
          } // e.g. killed by SIGTERM
          #[cfg(not(unix))]
          {
            true
          }
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
            let label = self
              .steps
              .get(self.current_step_index)
              .map(|(l, _)| {
                l.spans
                  .iter()
                  .map(|s| s.content.to_string())
                  .collect::<String>()
              })
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
        && commands.is_empty()
      {
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

impl Drop for InstallSteps<'_> {
  /// Never leave an installer child running after the UI goes away
  /// (e.g. Ctrl+C on the progress page while partitioning).
  fn drop(&mut self) {
    if let Some(child) = self.current_command.as_mut() {
      let _ = child.kill();
      let _ = child.wait();
    }
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
    Self {
      focused: false,
      selected_row: None,
      title: title.into(),
      headers,
      rows,
      widths,
    }
  }
  pub fn set_rows(&mut self, rows: Vec<Vec<String>>) {
    self.rows = rows;
    if let Some(idx) = self.selected_row
      && idx >= self.rows.len()
    {
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

/// Which pane of the package picker has keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerPane {
  Search,
  Selected,
  Available,
}

/// Complete package selection interface with dual-pane layout
///
/// - Left pane: currently selected packages
/// - Right pane: available packages with fuzzy search
/// - Tab: switch between panes, `/`: focus search, Enter: add/remove, `?`: help
pub struct PackagePicker {
  pub focused: bool,
  pub package_manager: PackageManager,
  pane: PickerPane,
  selected_cursor: usize,
  available_cursor: usize,
  title_selected: String,
  title_available: String,
  search_bar: LineEditor,
  help_modal: HelpModal<'static>,
}

impl PackagePicker {
  pub fn new(
    title_selected: &str,
    title_available: &str,
    selected_pkgs: &[String],
    available_pkgs: Vec<String>,
  ) -> Self {
    let package_manager = PackageManager::new(available_pkgs, selected_pkgs);
    let mut search_bar = LineEditor::new("Search", Some("Enter a package name..."));
    search_bar.focus();

    let bold = Some((Color::Yellow, Modifier::BOLD));
    let help_content = crate::styled_block(vec![
      vec![(bold, "Tab"), (None, " - Switch between lists and search")],
      vec![(bold, "↑/↓, j/k"), (None, " - Navigate package lists")],
      vec![
        (bold, "Enter"),
        (None, " - Add/remove package to/from selection"),
      ],
      vec![(bold, "/"), (None, " - Focus search bar")],
      vec![(bold, "?"), (None, " - Show this help")],
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
      pane: PickerPane::Search,
      selected_cursor: 0,
      available_cursor: 0,
      title_selected: title_selected.to_string(),
      title_available: title_available.to_string(),
      search_bar,
      help_modal,
    }
  }

  /// Replace the package model, keeping the current filter and cursors sane.
  pub fn set_package_manager(&mut self, mut pm: PackageManager) {
    pm.set_filter(self.search_bar.value().into());
    self.package_manager = pm;
    self.clamp_cursors();
  }

  pub fn get_selected_packages(&self) -> Vec<String> {
    self.package_manager.get_selected_packages()
  }

  fn set_pane(&mut self, pane: PickerPane) {
    self.pane = pane;
    if pane == PickerPane::Search {
      self.search_bar.focus();
    } else {
      self.search_bar.unfocus();
    }
  }

  /// Focus the search bar (used when opening the page)
  pub fn focus_search(&mut self) {
    self.set_pane(PickerPane::Search);
  }

  fn clamp_cursors(&mut self) {
    self.selected_cursor = self
      .selected_cursor
      .min(self.package_manager.selected_len().saturating_sub(1));
    self.available_cursor = self
      .available_cursor
      .min(self.package_manager.available().len().saturating_sub(1));
  }

  fn apply_search_filter(&mut self) {
    self
      .package_manager
      .set_filter(Some(self.search_bar.value()));
    self.available_cursor = 0;
  }

  fn render_list(
    &self,
    f: &mut Frame,
    area: Rect,
    title: &str,
    idxs: impl Iterator<Item = usize>,
    cursor: usize,
    focused: bool,
  ) {
    // Only materialise the rows that can be shown: a 20k-package list would
    // otherwise allocate 20k ListItems per frame.
    let visible = area.height.saturating_sub(2) as usize;
    let skip = cursor.saturating_sub(visible.saturating_sub(1));
    let items: Vec<ListItem> = idxs
      .skip(skip)
      .take(visible)
      .map(|i| ListItem::new(self.package_manager.name(i)))
      .collect();
    let border_color = if focused {
      Color::Yellow
    } else {
      Color::Indexed(242)
    };
    let list = List::new(items)
      .block(
        Block::default()
          .title(title)
          .borders(Borders::ALL)
          .border_style(Style::default().fg(border_color)),
      )
      .highlight_style(Style::default().bg(Color::Blue).fg(Color::White));
    let mut state = ListState::default();
    state.select(Some(cursor - skip));
    f.render_stateful_widget(list, area, &mut state);
  }
}

impl ConfigWidget for PackagePicker {
  fn render(&self, f: &mut Frame, area: Rect) {
    let hor_chunks = split_hor!(
      area,
      0,
      [Constraint::Percentage(50), Constraint::Percentage(50)]
    );
    let vert_chunks_left = split_vert!(
      hor_chunks[0],
      0,
      [Constraint::Length(5), Constraint::Min(0)]
    );
    let vert_chunks_right = split_vert!(
      hor_chunks[1],
      0,
      [Constraint::Length(5), Constraint::Min(0)]
    );

    let n_sel = self.package_manager.selected_len();
    let n_avail = self.package_manager.available().len();
    self.render_list(
      f,
      vert_chunks_left[1],
      &format!("{} ({n_sel})", self.title_selected),
      self.package_manager.selected(),
      self.selected_cursor,
      self.pane == PickerPane::Selected,
    );
    self.search_bar.render(f, vert_chunks_right[0]);
    self.render_list(
      f,
      vert_chunks_right[1],
      &format!("{} ({n_avail})", self.title_available),
      self.package_manager.available().iter().copied(),
      self.available_cursor,
      self.pane == PickerPane::Available,
    );
    self.help_modal.render(f, area);
  }

  fn handle_input(&mut self, event: KeyEvent) -> Signal {
    match event.code {
      KeyCode::Char('?') => {
        self.help_modal.toggle();
        return Signal::Wait;
      }
      KeyCode::Esc if self.help_modal.visible => {
        self.help_modal.hide();
        return Signal::Wait;
      }
      _ if self.help_modal.visible => return Signal::Wait,
      KeyCode::Left => {
        self.set_pane(match self.pane {
          PickerPane::Available | PickerPane::Search => PickerPane::Selected,
          PickerPane::Selected => PickerPane::Search,
        });
        return Signal::Wait;
      }
      KeyCode::Right => {
        self.set_pane(match self.pane {
          PickerPane::Selected | PickerPane::Search => PickerPane::Available,
          PickerPane::Available => PickerPane::Search,
        });
        return Signal::Wait;
      }
      KeyCode::Char('/') if self.pane != PickerPane::Search => {
        self.search_bar.clear();
        self.apply_search_filter();
        self.set_pane(PickerPane::Search);
        return Signal::Wait;
      }
      _ => {}
    }

    match self.pane {
      PickerPane::Search => match event.code {
        KeyCode::Enter | KeyCode::Tab | KeyCode::Down => self.set_pane(PickerPane::Available),
        KeyCode::Esc => {
          self.search_bar.clear();
          self.apply_search_filter();
          self.set_pane(PickerPane::Available);
        }
        _ => {
          self.search_bar.handle_input(event);
          self.apply_search_filter();
        }
      },
      PickerPane::Selected => match event.code {
        crate::ui_down!() => {
          if self.selected_cursor + 1 < self.package_manager.selected_len() {
            self.selected_cursor += 1;
          }
        }
        crate::ui_up!() => {
          if self.selected_cursor > 0 {
            self.selected_cursor -= 1;
          } else {
            self.set_pane(PickerPane::Search);
          }
        }
        KeyCode::Tab => self.set_pane(PickerPane::Available),
        KeyCode::Enter => {
          let idx = self.package_manager.selected().nth(self.selected_cursor);
          if let Some(idx) = idx {
            self.package_manager.deselect(idx);
            self.clamp_cursors();
          }
        }
        _ => {}
      },
      PickerPane::Available => match event.code {
        crate::ui_down!() => {
          if self.available_cursor + 1 < self.package_manager.available().len() {
            self.available_cursor += 1;
          }
        }
        crate::ui_up!() => {
          if self.available_cursor > 0 {
            self.available_cursor -= 1;
          } else {
            self.set_pane(PickerPane::Search);
          }
        }
        KeyCode::Tab => self.set_pane(PickerPane::Selected),
        KeyCode::Enter => {
          if let Some(&idx) = self.package_manager.available().get(self.available_cursor) {
            self.package_manager.select(idx);
            self.clamp_cursors();
          }
        }
        _ => {}
      },
    }
    Signal::Wait
  }

  fn focus(&mut self) {
    self.focused = true;
    self.set_pane(PickerPane::Search);
  }

  fn unfocus(&mut self) {
    self.focused = false;
    self.search_bar.unfocus();
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

/// Widget that displays streaming log output from a file
///
/// Features:
/// - Real-time log file monitoring (similar to 'tail -f')
/// - ANSI color code support for highlighted output
/// - Circular buffer to prevent memory growth
/// - Handles log rotation and file truncation
/// - Efficient incremental reading
pub struct LogBox<'a> {
  title: String,
  focused: bool,
  pub line_buf: VecDeque<Line<'a>>, // circular buffer
  max_buf_size: usize,              // max lines to keep
  reader: Option<File>,             // file we tail from
  inode: u64,                       // inode of `reader`, to notice replacement
  file_pos: u64,                    // current read offset
  log_path: Option<PathBuf>,        // path to the log file
  pending_bytes: Vec<u8>,           // bytes that didn't decode to valid UTF-8 yet
  pending_line: String,             // decoded but not yet newline-terminated text
  live_path: Option<PathBuf>,       // file with rows still being redrawn (progress bars)
  live_rows: Vec<String>,           // last contents of `live_path`
}

impl<'a> LogBox<'a> {
  pub fn new(title: String) -> Self {
    Self {
      title,
      focused: false,
      line_buf: VecDeque::with_capacity(512),
      max_buf_size: 1000,
      reader: None,
      inode: 0,
      file_pos: 0,
      log_path: None,
      pending_bytes: Vec::new(),
      pending_line: String::new(),
      live_path: None,
      live_rows: Vec::new(),
    }
  }

  /// Also show the rows the backend is still redrawing (download progress),
  /// published atomically in `path`. They are rendered below the log lines
  /// and disappear with the file.
  pub fn watch_live(&mut self, path: PathBuf) {
    self.live_path = Some(path);
  }

  fn poll_live(&mut self) {
    let Some(path) = &self.live_path else { return };
    self.live_rows = match std::fs::read_to_string(path) {
      Ok(s) => s.lines().map(str::to_string).collect(),
      Err(_) => Vec::new(),
    };
  }

  /// Start tailing `path` from its current end. The file is opened read-only
  /// and kept open; `poll_log` reads whatever was appended since last time.
  pub fn open_log<P: Into<PathBuf>>(&mut self, path: P) -> anyhow::Result<()> {
    let path = path.into();
    let file = OpenOptions::new().read(true).open(&path)?;
    let meta = file.metadata()?;
    self.file_pos = meta.len(); // start reading at EOF
    self.inode = meta.ino();
    self.reader = Some(file);
    self.log_path = Some(path);
    Ok(())
  }

  /// The installer backend recreates the log file when it starts (new inode).
  /// Reopen from the beginning when that happens so no output is missed.
  fn reopen_if_replaced(&mut self) {
    let Some(path) = &self.log_path else { return };
    let Ok(meta) = std::fs::metadata(path) else {
      return;
    };
    if meta.ino() == self.inode {
      return;
    }
    if let Ok(file) = OpenOptions::new().read(true).open(path) {
      self.reader = Some(file);
      self.inode = meta.ino();
      self.file_pos = 0;
      self.pending_bytes.clear();
      self.pending_line.clear();
    }
  }

  fn push_line(&mut self, line: Line<'a>) {
    self.line_buf.push_back(line);
    if self.line_buf.len() > self.max_buf_size {
      self.line_buf.pop_front();
    }
  }

  /// Call this every render tick to read any new bytes appended to the file.
  /// - Keeps a pending fragment if a line didn't end with '\n' yet.
  /// - Preserves ANSI colors by feeding raw text to `IntoText`.
  /// - Detects truncation/rotation and resets cleanly.
  pub fn poll_log(&mut self) -> anyhow::Result<()> {
    self.poll_live();
    self.reopen_if_replaced();
    let Some(f) = self.reader.as_mut() else {
      return Ok(());
    };

    let current_size = f.metadata()?.len();
    if current_size < self.file_pos {
      // truncated/rotated: start over
      self.file_pos = 0;
      self.pending_bytes.clear();
    }
    if current_size <= self.file_pos {
      return Ok(());
    }

    f.seek(SeekFrom::Start(self.file_pos))?;
    let mut buf = std::mem::take(&mut self.pending_bytes);
    f.take(current_size - self.file_pos).read_to_end(&mut buf)?;
    self.file_pos = current_size;

    // decode only the valid UTF-8 prefix; keep the remainder for next tick
    let valid_len = match std::str::from_utf8(&buf) {
      Ok(_) => buf.len(),
      Err(e) => e.valid_up_to(),
    };
    let decoded = String::from_utf8_lossy(&buf[..valid_len]).into_owned();
    self.pending_bytes.extend_from_slice(&buf[valid_len..]);

    // Stream-normalise: handle '\r' (overwrite), finalise on '\n', skip blanks
    for ch in decoded.chars() {
      match ch {
        '\r' => self.pending_line.clear(),
        '\n' => {
          let raw = std::mem::take(&mut self.pending_line);
          let stripped = strip_ansi_escapes::strip_str(&raw);
          if stripped.trim().is_empty() {
            continue;
          }
          let line = strip_log_prefix(&stripped);
          self.push_line(line);
        }
        _ => self.pending_line.push(ch),
      }
    }
    Ok(())
  }
}

/// Turn a log-file line into a display line for the Logs pane.
///
/// The logger writes `[ LEVEL ] HH:MM:SS message`; the file keeps that, but
/// on screen only the message is shown so it reads like the pacman output
/// around it. The level survives as colour: errors red, warnings yellow,
/// debug/trace dim.
fn strip_log_prefix(line: &str) -> Line<'static> {
  // "[ INFO ] 12:34:56 msg" -> level = "INFO", rest = "msg"
  let parsed = line.strip_prefix("[ ").and_then(|r| {
    let (level, rest) = r.split_once(" ] ")?;
    // 8-char timestamp followed by a space
    let msg = rest
      .get(9..)
      .filter(|_| rest.as_bytes().get(8) == Some(&b' '))?;
    Some((level.trim(), msg))
  });
  let (style, text) = match parsed {
    Some(("ERROR", msg)) => (
      Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
      msg,
    ),
    Some(("WARN", msg)) => (Style::default().fg(Color::Yellow), msg),
    Some(("DEBUG" | "TRACE", msg)) => (Style::default().fg(Color::Indexed(242)), msg),
    Some((_, msg)) => (Style::default(), msg),
    None => (Style::default(), line),
  };
  Line::from(Span::styled(text.to_string(), style))
}

impl<'a> ConfigWidget for LogBox<'a> {
  fn handle_input(&mut self, _key: KeyEvent) -> Signal {
    Signal::Wait
  }

  fn render(&self, f: &mut Frame, area: Rect) {
    // Live progress rows sit at the bottom; the newest log lines fill the rest.
    let rows = area.height.saturating_sub(2) as usize;
    let live: Vec<Line> = self
      .live_rows
      .iter()
      .filter(|r| !r.trim().is_empty())
      .map(|r| Line::from(Span::styled(r.clone(), Style::default().fg(Color::Cyan))))
      .collect();
    let live = &live[live.len().saturating_sub(rows)..];
    let log_rows = rows.saturating_sub(live.len());
    let start = self.line_buf.len().saturating_sub(log_rows);
    let mut visible: Vec<Line> = self.line_buf.iter().skip(start).cloned().collect();
    visible.extend_from_slice(live);

    let paragraph = Paragraph::new(visible)
      .block(
        Block::default()
          .title(self.title.as_str())
          .borders(Borders::ALL),
      )
      .wrap(Wrap { trim: true });

    f.render_widget(paragraph, area);
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

#[derive(Clone)]
enum Phase {
  Typing,
  FullIdle(Instant), // when it reached full length; store since
  Deleting,
}

pub struct FancyTicker {
  messages: &'static [&'static str],
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
  pub fn new(messages: &'static [&'static str]) -> Self {
    let first = messages.first().copied().unwrap_or("").to_string();
    Self {
      messages,
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
            if let Some(m) = self.messages.choose(&mut rng) {
              self.target = (*m).to_string();
            }

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
        && let Some(sub) = leet_map(c)
      {
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
#[cfg(test)]
mod tests {
  use super::*;
  use ratatui::crossterm::event::KeyModifiers;
  use std::io::Write;

  fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
  }

  #[test]
  fn line_editor_handles_multibyte_input() {
    let mut e = LineEditor::new("t", None::<&str>);
    for c in "héllo wörld".chars() {
      e.handle_input(key(KeyCode::Char(c)));
    }
    assert_eq!(e.value(), "héllo wörld");
    e.handle_input(key(KeyCode::Home));
    e.handle_input(key(KeyCode::Right));
    e.handle_input(key(KeyCode::Delete)); // delete 'é'
    assert_eq!(e.value(), "hllo wörld");
    e.handle_input(key(KeyCode::End));
    e.handle_input(key(KeyCode::Backspace));
    assert_eq!(e.value(), "hllo wörl");
    e.handle_input(key(KeyCode::Char('ß')));
    assert_eq!(e.value(), "hllo wörlß");
  }

  #[test]
  fn str_list_filter_tracks_selection() {
    let mut l = StrList::new(
      "t",
      vec![
        "Europe/Rome".into(),
        "Asia/Tokyo".into(),
        "Europe/Berlin".into(),
      ],
    );
    l.set_filter(Some("eur"));
    assert_eq!(l.filtered.len(), 2);
    assert!(l.selected_item().unwrap().starts_with("Europe"));
    l.next_item();
    l.handle_input(key(KeyCode::Enter));
    assert_eq!(
      l.committed.as_deref(),
      l.selected_item().map(String::as_str)
    );
    l.set_filter(None::<String>);
    assert_eq!(l.filtered.len(), 3);
  }

  #[test]
  fn log_box_tails_and_survives_file_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dory.log");
    std::fs::write(&path, "old\n").unwrap();
    let mut lb = LogBox::new("t".into());
    lb.open_log(&path).unwrap();
    lb.poll_log().unwrap();
    assert!(lb.line_buf.is_empty(), "starts at EOF");

    std::fs::OpenOptions::new()
      .append(true)
      .open(&path)
      .unwrap()
      .write_all(b"one\ntwo\npartial")
      .unwrap();
    lb.poll_log().unwrap();
    assert_eq!(lb.line_buf.len(), 2);

    // The backend removes and recreates the file: a new inode.
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, "fresh\n").unwrap();
    lb.poll_log().unwrap();
    assert_eq!(lb.line_buf.len(), 3);
    assert_eq!(lb.line_buf.back().unwrap().spans[0].content, "fresh");
  }

  #[test]
  fn log_prefix_is_stripped_but_level_colours_lines() {
    let l = strip_log_prefix("[ INFO ] 23:45:49 Set password for root");
    assert_eq!(l.spans[0].content, "Set password for root");
    assert_eq!(l.spans[0].style, Style::default());
    let e = strip_log_prefix("[ ERROR ] 23:45:50 Schedule enrollment  ERROR: failed");
    assert_eq!(e.spans[0].content, "Schedule enrollment  ERROR: failed");
    assert_eq!(e.spans[0].style.fg, Some(Color::Red));
    // lines not written by the logger (pacman output, step footer) are untouched
    let p = strip_log_prefix(" athena   120.0 KiB  1.2 MiB/s 00:00 [###] 100%");
    assert_eq!(
      p.spans[0].content,
      " athena   120.0 KiB  1.2 MiB/s 00:00 [###] 100%"
    );
    assert_eq!(
      strip_log_prefix("[ not a log line").spans[0].content,
      "[ not a log line"
    );
  }

  #[test]
  fn log_box_renders_live_rows_below_log() {
    use ratatui::{Terminal, backend::TestBackend};
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("dory.log");
    let live = dir.path().join("dory.live");
    std::fs::write(&log, "").unwrap();
    let mut lb = LogBox::new("Logs".into());
    lb.open_log(&log).unwrap();
    lb.watch_live(live.clone());
    std::fs::OpenOptions::new()
      .append(true)
      .open(&log)
      .unwrap()
      .write_all(b"[ INFO ] done pkg-a\n")
      .unwrap();
    std::fs::write(&live, "pkg-b  40% [####      ]\npkg-c  10% [#         ]").unwrap();
    lb.poll_log().unwrap();

    let mut term = Terminal::new(TestBackend::new(40, 6)).unwrap();
    term.draw(|f| lb.render(f, f.area())).unwrap();
    let screen = term
      .backend()
      .buffer()
      .content()
      .iter()
      .map(|c| c.symbol())
      .collect::<String>();
    assert!(screen.contains("done pkg-a"));
    assert!(screen.contains("pkg-b  40%"));
    assert!(screen.contains("pkg-c  10%"));

    // Live file removed at the end of the install: rows disappear.
    std::fs::remove_file(&live).unwrap();
    lb.poll_log().unwrap();
    term.draw(|f| lb.render(f, f.area())).unwrap();
    let screen = term
      .backend()
      .buffer()
      .content()
      .iter()
      .map(|c| c.symbol())
      .collect::<String>();
    assert!(!screen.contains("pkg-b"));
  }

  #[test]
  fn package_manager_select_deselect() {
    let mut pm = PackageManager::new(
      vec!["vim".into(), "nano".into(), "git".into()],
      &["git".into()],
    );
    assert_eq!(pm.get_selected_packages(), vec!["git"]);
    assert_eq!(pm.available().len(), 2);
    pm.set_filter(Some("vi".into()));
    assert_eq!(pm.available().len(), 1);
    let idx = pm.available()[0];
    pm.select(idx);
    assert_eq!(pm.get_selected_packages(), vec!["git", "vim"]);
    assert!(pm.available().is_empty());
    pm.deselect(idx);
    assert_eq!(pm.available().len(), 1);
  }
}
