use ratatui::{crossterm::event::KeyCode, layout::Constraint, text::Line};
use serde::{Deserialize, Serialize};

use crate::{
    installer::{Installer, Page, Signal},
    split_hor, split_vert, styled_block, ui_back, ui_close, ui_down, ui_enter, ui_up,
    widget::{Button, ConfigWidget, HelpModal, LineEditor, StrList, TableWidget, WidgetBox},
};

fn normalize_and_validate_username(raw: &str) -> Result<String, &'static str> {
    let s = raw.trim().to_lowercase();
    if s.is_empty() {
        return Err("Username cannot be empty");
    }
    if !s.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err("Special characters are not allowed (letters and digits only)");
    }
    Ok(s)
}

fn default_shell() -> String {
    "bash".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub username: String,
    pub password_hash: String,
    pub groups: Vec<String>,
    #[serde(default = "default_shell")]
    pub shell: String,
}

impl User {
    pub fn as_table_row(&self) -> Vec<String> {
        let groups = if self.groups.is_empty() {
            "<none>".to_string()
        } else {
            self.groups.join(", ")
        };
        vec![self.username.clone(), groups, self.shell.clone()]
    }

    pub fn has_wheel(&self) -> bool {
        self.groups.iter().any(|g| g == "wheel")
    }

    pub fn set_wheel(&mut self, enabled: bool) {
        if enabled {
            if !self.has_wheel() {
                self.groups.push("wheel".to_string());
                self.groups.sort();
                self.groups.dedup();
            }
        } else {
            self.groups.retain(|g| g != "wheel");
        }
    }
}

/* ===========================
   UserAccounts PAGE
   =========================== */

pub struct UserAccounts {
    pub user_table: TableWidget,
    pub buttons: WidgetBox,
    help_modal: HelpModal<'static>,
}

impl UserAccounts {
    pub fn new(users: Vec<User>) -> Self {
        let buttons = vec![Box::new(Button::new("Back")) as Box<dyn ConfigWidget>];
        let buttons = WidgetBox::button_menu(buttons);

        let widths = vec![
            Constraint::Percentage(33),
            Constraint::Percentage(33),
            Constraint::Percentage(33),
        ];
        let headers = vec![
            "Username".to_string(),
            "Groups".to_string(),
            "Shell".to_string(),
        ];

        let mut rows: Vec<Vec<String>> = users.into_iter().map(|u| u.as_table_row()).collect();
        rows.insert(0, vec!["Add a new user".into(), "".into(), "".into()]);

        let mut user_table = TableWidget::new("Users", widths, headers, rows);
        user_table.focus();

        let help_content = styled_block(vec![
            vec![
                (
                    Some((ratatui::style::Color::Yellow, ratatui::style::Modifier::BOLD)),
                    "↑/↓, j/k",
                ),
                (None, " - Navigate user list"),
            ],
            vec![
                (
                    Some((ratatui::style::Color::Yellow, ratatui::style::Modifier::BOLD)),
                    "Enter →, l",
                ),
                (None, " - Add new user or edit selected user"),
            ],
            vec![
                (
                    Some((ratatui::style::Color::Yellow, ratatui::style::Modifier::BOLD)),
                    "Tab",
                ),
                (None, " - Switch between user list and buttons"),
            ],
            vec![
                (
                    Some((ratatui::style::Color::Yellow, ratatui::style::Modifier::BOLD)),
                    "Esc, q, ←, h",
                ),
                (None, " - Return to main menu"),
            ],
            vec![
                (
                    Some((ratatui::style::Color::Yellow, ratatui::style::Modifier::BOLD)),
                    "?",
                ),
                (None, " - Show this help"),
            ],
            vec![(None, "")],
            vec![(None, "Create user accounts for your system.")],
            vec![
                (
                    None,
                    "Select 'Add a new user' to create accounts, or select",
                ),
            ],
            vec![(None, "an existing user to modify their settings.")],
        ]);

        let help_modal = HelpModal::new("User Accounts", help_content);

        Self {
            user_table,
            buttons,
            help_modal,
        }
    }

    pub fn display_widget(installer: &mut Installer) -> Option<Box<dyn ConfigWidget>> {
        let users = installer.users.clone();
        if users.is_empty() {
            return None;
        }
        Some(Box::new(TableWidget::new(
            "Users",
            vec![
                Constraint::Percentage(33),
                Constraint::Percentage(33),
                Constraint::Percentage(33),
            ],
            vec![
                "Username".to_string(),
                "Groups".to_string(),
                "Shell".to_string(),
            ],
            users.into_iter().map(|u| u.as_table_row()).collect(),
        )))
    }

    pub fn page_info<'a>() -> (String, Vec<Line<'a>>) {
        let title = "User Accounts".to_string();
        let description = vec![Line::from("Manage user accounts for the system.")];
        (title, description)
    }
}

impl Page for UserAccounts {
    fn render(
        &mut self,
        installer: &mut Installer,
        f: &mut ratatui::Frame,
        area: ratatui::prelude::Rect,
    ) {
        let chunks = split_vert!(
            area,
            1,
            [Constraint::Percentage(60), Constraint::Percentage(40),]
        );

        let mut rows: Vec<Vec<String>> = installer
            .users
            .clone()
            .into_iter()
            .map(|u| u.as_table_row())
            .collect();
        rows.insert(0, vec!["Add a new user".into(), "".into(), "".into()]);

        self.user_table.set_rows(rows);
        self.user_table.fix_selection();

        self.user_table.render(f, chunks[0]);
        self.buttons.render(f, chunks[1]);

        self.help_modal.render(f, area);
    }

    fn handle_input(
        &mut self,
        _installer: &mut Installer,
        event: ratatui::crossterm::event::KeyEvent,
    ) -> Signal {
        match event.code {
            KeyCode::Char('?') => {
                self.help_modal.toggle();
                return Signal::Wait;
            }
            ui_close!() if self.help_modal.visible => {
                self.help_modal.hide();
                return Signal::Wait;
            }
            _ if self.help_modal.visible => {
                return Signal::Wait;
            }
            _ => {}
        }

        if self.user_table.is_focused() {
            match event.code {
                ui_down!() => {
                    if !self.user_table.next_row() {
                        self.user_table.unfocus();
                        self.buttons.focus();
                        self.buttons.first_child();
                    }
                    Signal::Wait
                }
                ui_up!() => {
                    if !self.user_table.previous_row() {
                        self.user_table.unfocus();
                        self.buttons.focus();
                        self.buttons.last_child();
                    }
                    Signal::Wait
                }
                ui_enter!() => {
                    let Some(selected_user) = self.user_table.selected_row() else {
                        return Signal::Error(anyhow::anyhow!("No user selected"));
                    };
                    if selected_user == 0 {
                        Signal::Push(Box::new(AddUser::new()))
                    } else {
                        Signal::Push(Box::new(AlterUser::new(selected_user - 1)))
                    }
                }
                ui_back!() => Signal::Pop,
                _ => Signal::Wait,
            }
        } else if self.buttons.is_focused() {
            match event.code {
                ui_down!() => {
                    if !self.buttons.next_child() {
                        self.buttons.unfocus();
                        self.user_table.focus();
                        self.user_table.first_row();
                    }
                    Signal::Wait
                }
                ui_up!() => {
                    if !self.buttons.prev_child() {
                        self.buttons.unfocus();
                        self.user_table.focus();
                        self.user_table.last_row();
                    }
                    Signal::Wait
                }
                ui_enter!() => {
                    match self.buttons.selected_child() {
                        Some(0) => Signal::Pop, // Back
                        _ => Signal::Wait,
                    }
                }
                ui_back!() => Signal::Pop,
                _ => Signal::Wait,
            }
        } else {
            self.buttons.focus();
            Signal::Wait
        }
    }

    fn get_help_content(&self) -> (String, Vec<Line<'_>>) {
        let help_content = styled_block(vec![
            vec![
                (
                    Some((ratatui::style::Color::Yellow, ratatui::style::Modifier::BOLD)),
                    "↑/↓, j/k",
                ),
                (None, " - Navigate user list"),
            ],
            vec![
                (
                    Some((ratatui::style::Color::Yellow, ratatui::style::Modifier::BOLD)),
                    "Enter, →, l",
                ),
                (None, " - Add new user or edit selected user"),
            ],
            vec![
                (
                    Some((ratatui::style::Color::Yellow, ratatui::style::Modifier::BOLD)),
                    "Tab",
                ),
                (None, " - Switch between user list and buttons"),
            ],
            vec![
                (
                    Some((ratatui::style::Color::Yellow, ratatui::style::Modifier::BOLD)),
                    "Esc, q, ←, h",
                ),
                (None, " - Return to main menu"),
            ],
            vec![
                (
                    Some((ratatui::style::Color::Yellow, ratatui::style::Modifier::BOLD)),
                    "?",
                ),
                (None, " - Show this help"),
            ],
            vec![(None, "")],
            vec![(None, "Create user accounts for your system.")],
            vec![
                (
                    None,
                    "Select 'Add a new user' to create accounts, or select",
                ),
            ],
            vec![(None, "an existing user to modify their settings.")],
        ]);
        ("User Accounts".to_string(), help_content)
    }
}

/* ===========================
   AddUser PAGE
   =========================== */

pub struct AddUser {
    name_input: LineEditor,
    pass_input: LineEditor,
    pass_confirm: LineEditor,
    shell_list: StrList,
    selected_shell: String,

    sudo_list: StrList, // "Yes"/"No"

    help_modal: HelpModal<'static>,
    username: Option<String>,
    finished: bool,
}

impl AddUser {
    pub fn new() -> Self {
        let mut name_input = LineEditor::new("Username", None::<&str>);
        name_input.focus();

        let shell_list = StrList::new(
            "Shell",
            vec!["bash".to_string(), "fish".to_string(), "zsh".to_string()],
        );

        let sudo_list = StrList::new(
            "Awaken sudo power",
            vec!["Yes".to_string(), "No".to_string()],
        );

        let help_content = styled_block(vec![
            vec![
                (
                    Some((
                        ratatui::style::Color::Yellow,
                        ratatui::style::Modifier::BOLD,
                    )),
                    "Tab / Shift+Tab",
                ),
                (None, " - Move between fields"),
            ],
            vec![
                (
                    Some((
                        ratatui::style::Color::Yellow,
                        ratatui::style::Modifier::BOLD,
                    )),
                    "↑/↓, j/k",
                ),
                (None, " - Navigate shell / sudo"),
            ],
            vec![
                (
                    Some((
                        ratatui::style::Color::Yellow,
                        ratatui::style::Modifier::BOLD,
                    )),
                    "Enter",
                ),
                (None, " - Advance / create user"),
            ],
            vec![
                (
                    Some((
                        ratatui::style::Color::Yellow,
                        ratatui::style::Modifier::BOLD,
                    )),
                    "Esc",
                ),
                (None, " - Cancel and return"),
            ],
        ]);

        let help_modal = HelpModal::new("Add User", help_content);

        Self {
            name_input,
            pass_input: LineEditor::new("Password", None::<&str>).secret(true),
            pass_confirm: LineEditor::new("Confirm Password", None::<&str>).secret(true),
            shell_list,
            selected_shell: "bash".into(),
            sudo_list,
            help_modal,
            username: None,
            finished: false,
        }
    }

    fn cycle_forward(&mut self) {
        // name -> pass -> confirm -> shell -> sudo -> wrap
        if self.name_input.is_focused() {
            let entered = self
                .name_input
                .get_value()
                .and_then(|s| s.as_str().map(|s| s.to_owned()))
                .unwrap_or_default();

            match normalize_and_validate_username(&entered) {
                Ok(norm) => {
                    self.name_input.clear_error();
                    self.username = Some(norm);
                    self.name_input.unfocus();
                    self.pass_input.focus();
                }
                Err(msg) => {
                    self.name_input.error(msg);
                }
            }
        } else if self.pass_input.is_focused() {
            self.pass_input.unfocus();
            self.pass_confirm.focus();
        } else if self.pass_confirm.is_focused() {
            self.pass_confirm.unfocus();
            self.shell_list.focus();
        } else if self.shell_list.is_focused() {
            self.shell_list.unfocus();
            self.sudo_list.focus();
        } else if self.sudo_list.is_focused() {
            self.sudo_list.unfocus();
            self.name_input.focus();
        } else {
            self.name_input.focus();
        }
    }

    fn cycle_backward(&mut self) {
        // reverse order
        if self.name_input.is_focused() {
            self.name_input.unfocus();
            self.sudo_list.focus();
        } else if self.pass_input.is_focused() {
            self.pass_input.unfocus();
            self.name_input.focus();
        } else if self.pass_confirm.is_focused() {
            self.pass_confirm.unfocus();
            self.pass_input.focus();
        } else if self.shell_list.is_focused() {
            self.shell_list.unfocus();
            self.pass_confirm.focus();
        } else if self.sudo_list.is_focused() {
            self.sudo_list.unfocus();
            self.shell_list.focus();
        } else {
            self.name_input.focus();
        }
    }

    fn finalize_user(&mut self, installer: &mut Installer) -> anyhow::Result<()> {
        // username
        let username = match self.username.clone() {
            Some(u) => u,
            None => {
                let entered_now = self
                    .name_input
                    .get_value()
                    .and_then(|s| s.as_str().map(|s| s.to_owned()))
                    .unwrap_or_default();
                let normalized = normalize_and_validate_username(&entered_now)
                    .map_err(|msg| anyhow::anyhow!(msg))?;
                if installer.users.iter().any(|u| u.username == normalized) {
                    self.name_input.error("User already exists");
                    return Err(anyhow::anyhow!("duplicate user"));
                }
                self.username = Some(normalized.clone());
                normalized
            }
        };

        if installer.users.iter().any(|u| u.username == username) {
            self.name_input.error("User already exists");
            return Err(anyhow::anyhow!("duplicate user"));
        }

        // password + confirm
        let pass = self
            .pass_input
            .get_value()
            .and_then(|v| v.as_str().map(|s| s.to_owned()))
            .unwrap_or_default();
        let confirm = self
            .pass_confirm
            .get_value()
            .and_then(|v| v.as_str().map(|s| s.to_owned()))
            .unwrap_or_default();

        if pass.is_empty() {
            self.pass_input.error("Password cannot be empty");
            return Err(anyhow::anyhow!("empty pass"));
        }
        if confirm.is_empty() {
            self.pass_confirm
                .error("Password confirmation cannot be empty");
            return Err(anyhow::anyhow!("empty confirm"));
        }
        if pass != confirm {
            self.pass_confirm.clear();
            self.pass_confirm.error("Passwords do not match");
            return Err(anyhow::anyhow!("mismatch"));
        }

        // shell
        if let Some(sel) = self.shell_list.selected_item() {
            self.selected_shell = sel.to_string();
        }

        // sudo via sudo_list
        let sudo_allowed = self
            .sudo_list
            .selected_item()
            .map(|s| s == "Yes")
            .unwrap_or(false);

        // hash
        let hashed = super::RootPassword::mkpasswd(pass)?;

        // groups
        let mut groups = vec![];
        groups.push("docker".to_string());
        if sudo_allowed {
            groups.push("wheel".to_string());
        }

        installer.users.push(User {
            username,
            password_hash: hashed,
            groups,
            shell: self.selected_shell.clone(),
        });

        self.finished = true;
        Ok(())
    }
}

impl Default for AddUser {
    fn default() -> Self {
        Self::new()
    }
}

impl Page for AddUser {
    fn render(
        &mut self,
        _installer: &mut Installer,
        f: &mut ratatui::Frame,
        area: ratatui::prelude::Rect,
    ) {
        // center column
        let hor_chunks = split_hor!(
            area,
            1,
            [
                Constraint::Percentage(25),
                Constraint::Percentage(50),
                Constraint::Percentage(25),
            ]
        );

        // username / pass / confirm / shell / sudo / filler
        //
        // Note: we intentionally give shell+sudo a *bit* of vertical room,
        // then we crop when rendering so they look snug.
        let vert_chunks = split_vert!(
            hor_chunks[1],
            0,
            [
                Constraint::Length(5), // username
                Constraint::Length(5), // pass
                Constraint::Length(5), // confirm
                Constraint::Length(6), // shell list visual height (cropped below)
                Constraint::Length(6), // sudo list visual height (cropped below)
                Constraint::Min(0),
            ]
        );

        self.name_input.render(f, vert_chunks[0]);
        self.pass_input.render(f, vert_chunks[1]);
        self.pass_confirm.render(f, vert_chunks[2]);

        // ---- compact shell box render ----
        let shell_full = vert_chunks[3];
        let shell_compact = ratatui::prelude::Rect {
            x: shell_full.x,
            y: shell_full.y,
            width: shell_full.width,
            // allow 3 entries (bash, fish, zsh)
            height: shell_full.height.min(5),
        };
        self.shell_list.render(f, shell_compact);

        // ---- compact sudo box render ----
        let sudo_full = vert_chunks[4];
        let sudo_compact = ratatui::prelude::Rect {
            x: sudo_full.x,
            y: sudo_full.y,
            width: sudo_full.width,
            // only Yes / No, so tighter is fine
            height: sudo_full.height.min(4),
        };
        self.sudo_list.render(f, sudo_compact);

        self.help_modal.render(f, area);
    }

    fn handle_input(
        &mut self,
        installer: &mut Installer,
        event: ratatui::crossterm::event::KeyEvent,
    ) -> Signal {
        if self.finished {
            return Signal::Pop;
        }

        match event.code {
            KeyCode::Char('?') => {
                self.help_modal.toggle();
                return Signal::Wait;
            }
            ui_close!() if self.help_modal.visible => {
                self.help_modal.hide();
                return Signal::Wait;
            }
            KeyCode::Esc => {
                return Signal::Pop;
            }
            _ if self.help_modal.visible => {
                return Signal::Wait;
            }
            _ => {}
        }

        if event.code == KeyCode::Tab {
            self.cycle_forward();
            return Signal::Wait;
        } else if event.code == KeyCode::BackTab {
            self.cycle_backward();
            return Signal::Wait;
        }

        if self.name_input.is_focused() {
            match event.code {
                KeyCode::Enter => {
                    let entered_owned = self
                        .name_input
                        .get_value()
                        .and_then(|s| s.as_str().map(|s| s.to_owned()))
                        .unwrap_or_default();

                    let normalized = match normalize_and_validate_username(&entered_owned) {
                        Ok(n) => n,
                        Err(msg) => {
                            self.name_input.error(msg);
                            return Signal::Wait;
                        }
                    };

                    if installer.users.iter().any(|u| u.username == normalized) {
                        self.name_input.error("User already exists");
                        return Signal::Wait;
                    }

                    self.name_input.clear_error();
                    self.username = Some(normalized);
                    self.name_input.unfocus();
                    self.pass_input.focus();
                    Signal::Wait
                }
                _ => self.name_input.handle_input(event),
            }
        } else if self.pass_input.is_focused() {
            match event.code {
                KeyCode::Enter => {
                    if let Some(pass) = self.pass_input.get_value() {
                        let Some(pass) = pass.as_str() else {
                            self.pass_input.error("Password cannot be empty");
                            return Signal::Wait;
                        };
                        if pass.is_empty() {
                            self.pass_input.error("Password cannot be empty");
                            return Signal::Wait;
                        }
                        self.pass_input.clear_error();
                        self.pass_input.unfocus();
                        self.pass_confirm.focus();
                        Signal::Wait
                    } else {
                        self.pass_input.error("Password cannot be empty");
                        Signal::Wait
                    }
                }
                _ => self.pass_input.handle_input(event),
            }
        } else if self.pass_confirm.is_focused() {
            match event.code {
                KeyCode::Enter => {
                    if let Some(pass) = self.pass_input.get_value() {
                        let Some(pass) = pass.as_str() else {
                            self.pass_input.error("Password cannot be empty");
                            return Signal::Wait;
                        };
                        if let Some(confirm) = self.pass_confirm.get_value() {
                            let Some(confirm) = confirm.as_str() else {
                                self.pass_confirm
                                    .error("Password confirmation cannot be empty");
                                return Signal::Wait;
                            };
                            if pass != confirm {
                                self.pass_confirm.clear();
                                self.pass_confirm.error("Passwords do not match");
                                self.pass_input.unfocus();
                                self.pass_confirm.focus();
                                return Signal::Wait;
                            }

                            self.pass_input.clear_error();
                            self.pass_confirm.clear_error();
                            self.pass_confirm.unfocus();
                            self.shell_list.focus();
                            Signal::Wait
                        } else {
                            self.pass_confirm
                                .error("Password confirmation cannot be empty");
                            Signal::Wait
                        }
                    } else {
                        self.pass_input.error("Password cannot be empty");
                        Signal::Wait
                    }
                }
                _ => self.pass_confirm.handle_input(event),
            }
        } else if self.shell_list.is_focused() {
            match event.code {
                ui_down!() => {
                    self.shell_list.next_item();
                    Signal::Wait
                }
                ui_up!() => {
                    self.shell_list.previous_item();
                    Signal::Wait
                }
                KeyCode::Enter => {
                    if let Some(sel) = self.shell_list.selected_item() {
                        self.selected_shell = sel.to_string();
                    }
                    self.shell_list.unfocus();
                    self.sudo_list.focus();
                    Signal::Wait
                }
                KeyCode::Esc => {
                    self.shell_list.unfocus();
                    self.pass_confirm.focus();
                    Signal::Wait
                }
                _ => Signal::Wait,
            }
        } else if self.sudo_list.is_focused() {
            match event.code {
                ui_down!() => {
                    self.sudo_list.next_item();
                    Signal::Wait
                }
                ui_up!() => {
                    self.sudo_list.previous_item();
                    Signal::Wait
                }
                KeyCode::Enter => {
                    match self.finalize_user(installer) {
                        Ok(()) => {
                            self.finished = true;
                            Signal::Pop
                        }
                        Err(e) => Signal::Error(e),
                    }
                }
                KeyCode::Esc => {
                    self.sudo_list.unfocus();
                    self.shell_list.focus();
                    Signal::Wait
                }
                _ => Signal::Wait,
            }
        } else {
            self.name_input.focus();
            Signal::Wait
        }
    }

    fn get_help_content(&self) -> (String, Vec<Line<'_>>) {
        let help_content = styled_block(vec![
            vec![
                (
                    Some((ratatui::style::Color::Yellow, ratatui::style::Modifier::BOLD)),
                    "Tab / Shift+Tab",
                ),
                (None, " - Move between fields"),
            ],
            vec![
                (
                    Some((ratatui::style::Color::Yellow, ratatui::style::Modifier::BOLD)),
                    "↑/↓, j/k",
                ),
                (None, " - Navigate shell/sudo list"),
            ],
            vec![
                (
                    Some((ratatui::style::Color::Yellow, ratatui::style::Modifier::BOLD)),
                    "Enter",
                ),
                (None, " - Confirm / finalize new user"),
            ],
            vec![
                (
                    Some((ratatui::style::Color::Yellow, ratatui::style::Modifier::BOLD)),
                    "Esc",
                ),
                (None, " - Cancel and return"),
            ],
            vec![(None, "")],
            vec![(None, "Create a new user account and choose sudo.")],
        ]);
        ("Add User".to_string(), help_content)
    }
}

/* ===========================
   AlterUser PAGE
   =========================== */

pub struct AlterUser {
    pub selected_user: usize,

    pub buttons: WidgetBox,

    pub name_input: LineEditor,

    pub pass_input: LineEditor,
    pub pass_confirm: LineEditor,

    pub shell_list: StrList,

    pub sudo_list: StrList,
    sudo_mode: bool,

    confirming_delete: bool,

    help_modal: HelpModal<'static>,
}

impl AlterUser {
    pub fn new(selected_user_idx: usize) -> Self {
        let buttons_children = vec![
            Box::new(Button::new("Change username")) as Box<dyn ConfigWidget>,
            Box::new(Button::new("Change password")) as Box<dyn ConfigWidget>,
            Box::new(Button::new("Change shell")) as Box<dyn ConfigWidget>,
            Box::new(Button::new("Change sudo (wheel)")) as Box<dyn ConfigWidget>,
            Box::new(Button::new("Delete user")) as Box<dyn ConfigWidget>,
        ];
        let mut buttons = WidgetBox::button_menu(buttons_children);
        buttons.focus();

        let shell_list = StrList::new(
            "Shell",
            vec!["bash".to_string(), "fish".to_string(), "zsh".to_string()],
        );

        let mut sudo_list = StrList::new(
            "Administrative rights (sudo via wheel)",
            vec!["Yes".to_string(), "No".to_string()],
        );
        sudo_list.unfocus();

        let help_content = styled_block(vec![
            vec![
                (
                    Some((ratatui::style::Color::Yellow, ratatui::style::Modifier::BOLD)),
                    "↑/↓, j/k",
                ),
                (None, " - Navigate menu / lists"),
            ],
            vec![
                (
                    Some((ratatui::style::Color::Yellow, ratatui::style::Modifier::BOLD)),
                    "Enter, →, l",
                ),
                (None, " - Select / apply change"),
            ],
            vec![
                (
                    Some((ratatui::style::Color::Yellow, ratatui::style::Modifier::BOLD)),
                    "Tab",
                ),
                (None, " - Switch password fields"),
            ],
            vec![
                (
                    Some((ratatui::style::Color::Yellow, ratatui::style::Modifier::BOLD)),
                    "Esc, q, ←, h",
                ),
                (None, " - Go back / cancel"),
            ],
        ]);
        let help_modal = HelpModal::new("Alter User", help_content);

        Self {
            selected_user: selected_user_idx,
            buttons,
            name_input: LineEditor::new("New username", None::<&str>),
            pass_input: LineEditor::new("New password", None::<&str>).secret(true),
            pass_confirm: LineEditor::new("Confirm password", None::<&str>).secret(true),
            shell_list,
            sudo_list,
            sudo_mode: false,
            confirming_delete: false,
            help_modal,
        }
    }

    fn render_main_menu(&mut self, f: &mut ratatui::Frame, area: ratatui::prelude::Rect) {
        let vert_chunks = split_vert!(
            area,
            1,
            [Constraint::Percentage(50), Constraint::Percentage(50)]
        );
        let hor_chunks = split_hor!(
            vert_chunks[0],
            1,
            [
                Constraint::Percentage(40),
                Constraint::Percentage(20),
                Constraint::Percentage(40),
            ]
        );
        self.buttons.render(f, hor_chunks[1]);
    }

    fn render_name_change(&mut self, f: &mut ratatui::Frame, area: ratatui::prelude::Rect) {
        let chunks = split_vert!(area, 1, [Constraint::Length(7), Constraint::Min(0)]);
        let hor_chunks = split_hor!(
            chunks[0],
            1,
            [
                Constraint::Percentage(25),
                Constraint::Percentage(50),
                Constraint::Percentage(25),
            ]
        );
        self.name_input.render(f, hor_chunks[1]);
    }

    fn render_pass_change(&mut self, f: &mut ratatui::Frame, area: ratatui::prelude::Rect) {
        let chunks = split_vert!(
            area,
            1,
            [
                Constraint::Length(7),
                Constraint::Length(7),
                Constraint::Min(0),
            ]
        );
        let hor_chunks1 = split_hor!(
            chunks[0],
            1,
            [
                Constraint::Percentage(25),
                Constraint::Percentage(50),
                Constraint::Percentage(25),
            ]
        );
        let hor_chunks2 = split_hor!(
            chunks[1],
            1,
            [
                Constraint::Percentage(25),
                Constraint::Percentage(50),
                Constraint::Percentage(25),
            ]
        );
        self.pass_input.render(f, hor_chunks1[1]);
        self.pass_confirm.render(f, hor_chunks2[1]);
    }

    fn render_select_shell(&mut self, f: &mut ratatui::Frame, area: ratatui::prelude::Rect) {
        let hor_chunks = split_hor!(
            area,
            1,
            [
                Constraint::Percentage(25),
                Constraint::Percentage(50),
                Constraint::Percentage(25),
            ]
        );
        let vert = split_vert!(hor_chunks[1], 1, [Constraint::Length(6), Constraint::Min(0)]);
      
        // compact draw for shell_list (3 items, so need 5 rows)
        let shell_full = vert[0];
        let shell_compact = ratatui::prelude::Rect {
            x: shell_full.x,
            y: shell_full.y,
            width: shell_full.width,
            height: shell_full.height.min(5),
        };
        self.shell_list.render(f, shell_compact);
    }

    fn render_sudo_mode(&mut self, f: &mut ratatui::Frame, area: ratatui::prelude::Rect) {
        let hor_chunks = split_hor!(
            area,
            1,
            [
                Constraint::Percentage(25),
                Constraint::Percentage(50),
                Constraint::Percentage(25),
            ]
        );
        let vert_full = split_vert!(hor_chunks[1], 1, [Constraint::Length(6), Constraint::Min(0)]);
      
        let full = vert_full[0];
        let compact = ratatui::prelude::Rect {
            x: full.x,
            y: full.y,
            width: full.width,
            height: full.height.min(4), // Yes / No fits here
        };
      
        self.sudo_list.render(f, compact);
    }

    fn handle_input_main_menu(
        &mut self,
        installer: &mut Installer,
        event: ratatui::crossterm::event::KeyEvent,
    ) -> Signal {
        if self.confirming_delete && event.code != KeyCode::Enter {
            self.confirming_delete = false;
            let buttons = vec![
                Box::new(Button::new("Change username")) as Box<dyn ConfigWidget>,
                Box::new(Button::new("Change password")) as Box<dyn ConfigWidget>,
                Box::new(Button::new("Change shell")) as Box<dyn ConfigWidget>,
                Box::new(Button::new("Change sudo (wheel)")) as Box<dyn ConfigWidget>,
                Box::new(Button::new("Delete user")) as Box<dyn ConfigWidget>,
            ];
            self.buttons.set_children_inplace(buttons);
        }

        match event.code {
            ui_down!() => {
                if !self.buttons.next_child() {
                    self.buttons.first_child();
                }
                Signal::Wait
            }
            ui_up!() => {
                if !self.buttons.prev_child() {
                    self.buttons.last_child();
                }
                Signal::Wait
            }
            KeyCode::Enter => {
                match self.buttons.selected_child() {
                    Some(0) => {
                        // Change username
                        self.buttons.unfocus();
                        self.name_input.focus();
                        Signal::Wait
                    }
                    Some(1) => {
                        // Change password
                        self.buttons.unfocus();
                        self.pass_input.focus();
                        Signal::Wait
                    }
                    Some(2) => {
                        // Change shell
                        self.buttons.unfocus();
                        if self.selected_user < installer.users.len() {
                            let cur = installer.users[self.selected_user].shell.clone();
                            self.shell_list.first_item();
                            let mut scanned = 0usize;
                            while self.shell_list.selected_item() != Some(&cur) && scanned < 16 {
                                if !self.shell_list.next_item() {
                                    break;
                                }
                                scanned += 1;
                            }
                        }
                        self.shell_list.focus();
                        Signal::Wait
                    }
                    Some(3) => {
                        // Change sudo (wheel)
                        if self.selected_user < installer.users.len() {
                            let cur_has = installer.users[self.selected_user].has_wheel();
                            self.sudo_list.selected_idx = if cur_has { 0 } else { 1 };
                        }
                        self.buttons.unfocus();
                        self.sudo_list.focus();
                        self.sudo_mode = true;
                        Signal::Wait
                    }
                    Some(4) => {
                        // Delete user
                        if !self.confirming_delete {
                            self.confirming_delete = true;
                            let buttons = vec![
                                Box::new(Button::new("Change username")) as Box<dyn ConfigWidget>,
                                Box::new(Button::new("Change password")) as Box<dyn ConfigWidget>,
                                Box::new(Button::new("Change shell")) as Box<dyn ConfigWidget>,
                                Box::new(Button::new("Change sudo (wheel)")) as Box<dyn ConfigWidget>,
                                Box::new(Button::new("Really?")) as Box<dyn ConfigWidget>,
                            ];
                            self.buttons.set_children_inplace(buttons);
                            Signal::Wait
                        } else {
                            if self.selected_user < installer.users.len() {
                                installer.users.remove(self.selected_user);
                            }
                            Signal::Pop
                        }
                    }
                    _ => Signal::Wait,
                }
            }
            ui_back!() => Signal::Pop,
            _ => Signal::Wait,
        }
    }

    fn handle_input_name_change(
        &mut self,
        installer: &mut Installer,
        event: ratatui::crossterm::event::KeyEvent,
    ) -> Signal {
        match event.code {
            KeyCode::Enter => {
                let entered_owned = self
                    .name_input
                    .get_value()
                    .and_then(|s| s.as_str().map(|s| s.to_owned()))
                    .unwrap_or_default();

                let normalized = match normalize_and_validate_username(&entered_owned) {
                    Ok(n) => n,
                    Err(msg) => {
                        self.name_input.error(msg);
                        return Signal::Wait;
                    }
                };

                if self.selected_user < installer.users.len()
                    && installer.users[self.selected_user].username == normalized
                {
                    self.name_input.unfocus();
                    self.buttons.focus();
                    return Signal::Wait;
                }

                if installer
                    .users
                    .iter()
                    .enumerate()
                    .any(|(i, u)| i != self.selected_user && u.username == normalized)
                {
                    self.name_input.error("User already exists");
                    return Signal::Wait;
                }

                if self.selected_user < installer.users.len() {
                    installer.users[self.selected_user].username = normalized;
                }
                self.name_input.unfocus();
                self.buttons.focus();
                Signal::Wait
            }
            ui_close!() => {
                self.name_input.unfocus();
                self.buttons.focus();
                Signal::Wait
            }
            _ => self.name_input.handle_input(event),
        }
    }

    fn handle_input_pass_change(
        &mut self,
        installer: &mut Installer,
        event: ratatui::crossterm::event::KeyEvent,
    ) -> Signal {
        if self.pass_input.is_focused() {
            match event.code {
                KeyCode::Tab => {
                    self.pass_input.clear_error();
                    self.pass_input.unfocus();
                    self.pass_confirm.focus();
                    Signal::Wait
                }
                KeyCode::Enter => {
                    if let Some(pass) = self.pass_input.get_value() {
                        let Some(pass) = pass.as_str() else {
                            self.pass_input.error("Password cannot be empty");
                            return Signal::Wait;
                        };
                        if pass.is_empty() {
                            self.pass_input.error("Password cannot be empty");
                            return Signal::Wait;
                        }
                        self.pass_input.clear_error();
                        self.pass_input.unfocus();
                        self.pass_confirm.focus();
                        Signal::Wait
                    } else {
                        self.pass_input.error("Password cannot be empty");
                        Signal::Wait
                    }
                }
                KeyCode::Esc => {
                    self.pass_input.unfocus();
                    self.buttons.focus();
                    Signal::Wait
                }
                _ => self.pass_input.handle_input(event),
            }
        } else if self.pass_confirm.is_focused() {
            match event.code {
                KeyCode::Tab => {
                    self.pass_confirm.unfocus();
                    self.pass_input.focus();
                    Signal::Wait
                }
                KeyCode::Esc => {
                    self.pass_confirm.unfocus();
                    self.buttons.focus();
                    Signal::Wait
                }
                KeyCode::Enter => {
                    if let Some(pass) = self.pass_input.get_value() {
                        let Some(pass) = pass.as_str() else {
                            self.pass_input.error("Password cannot be empty");
                            return Signal::Wait;
                        };
                        if let Some(confirm) = self.pass_confirm.get_value() {
                            let Some(confirm) = confirm.as_str() else {
                                self.pass_confirm.error(
                                    "Password confirmation cannot be empty",
                                );
                                return Signal::Wait;
                            };
                            if pass != confirm {
                                self.pass_confirm.clear();
                                self.pass_confirm.error("Passwords do not match");
                                self.pass_input.unfocus();
                                self.pass_confirm.focus();
                                return Signal::Wait;
                            }
                            let hashed = match super::RootPassword::mkpasswd(pass.to_string()) {
                                Ok(h) => h,
                                Err(e) => {
                                    return Signal::Error(anyhow::anyhow!(
                                        "Failed to hash password: {e}"
                                    ));
                                }
                            };
                            if self.selected_user < installer.users.len() {
                                installer.users[self.selected_user].password_hash = hashed;
                            }
                            self.pass_confirm.unfocus();
                            self.buttons.focus();
                            Signal::Wait
                        } else {
                            self.pass_confirm.error(
                                "Password confirmation cannot be empty",
                            );
                            Signal::Wait
                        }
                    } else {
                        self.pass_input.error("Password cannot be empty");
                        Signal::Wait
                    }
                }
                _ => self.pass_confirm.handle_input(event),
            }
        } else {
            self.pass_input.focus();
            Signal::Wait
        }
    }

    fn handle_input_sudo_mode(
        &mut self,
        installer: &mut Installer,
        event: ratatui::crossterm::event::KeyEvent,
    ) -> Signal {
        match event.code {
            ui_down!() => {
                self.sudo_list.next_item();
                Signal::Wait
            }
            ui_up!() => {
                self.sudo_list.previous_item();
                Signal::Wait
            }
            KeyCode::Enter => {
                let choice_yes = self
                    .sudo_list
                    .selected_item()
                    .map(|s| s == "Yes")
                    .unwrap_or(false);

                if self.selected_user < installer.users.len() {
                    installer.users[self.selected_user].set_wheel(choice_yes);
                }

                self.sudo_list.unfocus();
                self.sudo_mode = false;
                self.buttons.focus();
                Signal::Wait
            }
            KeyCode::Esc => {
                self.sudo_list.unfocus();
                self.sudo_mode = false;
                self.buttons.focus();
                Signal::Wait
            }
            _ => Signal::Wait,
        }
    }
}

impl Page for AlterUser {
    fn render(
        &mut self,
        _installer: &mut Installer,
        f: &mut ratatui::Frame,
        area: ratatui::prelude::Rect,
    ) {
        if self.sudo_mode && self.sudo_list.is_focused() {
            self.render_sudo_mode(f, area);
        } else if self.buttons.is_focused() {
            self.render_main_menu(f, area);
        } else if self.name_input.is_focused() {
            self.render_name_change(f, area);
        } else if self.pass_input.is_focused() || self.pass_confirm.is_focused() {
            self.render_pass_change(f, area);
        } else if self.shell_list.is_focused() {
            self.render_select_shell(f, area);
        } else {
            self.buttons.focus();
            self.render_main_menu(f, area);
        }

        self.help_modal.render(f, area);
    }

    fn handle_input(
        &mut self,
        installer: &mut Installer,
        event: ratatui::crossterm::event::KeyEvent,
    ) -> Signal {
        match event.code {
            KeyCode::Char('?') => {
                self.help_modal.toggle();
                return Signal::Wait;
            }
            ui_close!() if self.help_modal.visible => {
                self.help_modal.hide();
                return Signal::Wait;
            }
            _ if self.help_modal.visible => {
                return Signal::Wait;
            }
            _ => {}
        }

        if self.sudo_mode && self.sudo_list.is_focused() {
            return self.handle_input_sudo_mode(installer, event);
        }

        if self.buttons.is_focused() {
            self.handle_input_main_menu(installer, event)
        } else if self.name_input.is_focused() {
            self.handle_input_name_change(installer, event)
        } else if self.pass_input.is_focused() || self.pass_confirm.is_focused() {
            self.handle_input_pass_change(installer, event)
        } else if self.shell_list.is_focused() {
            match event.code {
                ui_down!() => {
                    if !self.shell_list.next_item() {
                        self.shell_list.first_item();
                    }
                    Signal::Wait
                }
                ui_up!() => {
                    if !self.shell_list.previous_item() {
                        self.shell_list.last_item();
                    }
                    Signal::Wait
                }
                KeyCode::Esc | KeyCode::Tab | KeyCode::BackTab => {
                    self.shell_list.unfocus();
                    self.buttons.focus();
                    Signal::Wait
                }
                KeyCode::Enter => {
                    if let Some(sel) = self.shell_list.selected_item()
                        && self.selected_user < installer.users.len()
                    {
                        installer.users[self.selected_user].shell = sel.to_string();
                    }
                    self.shell_list.unfocus();
                    self.buttons.focus();
                    Signal::Wait
                }
                _ => Signal::Wait,
            }
        } else {
            self.buttons.focus();
            Signal::Wait
        }
    }

    fn get_help_content(&self) -> (String, Vec<Line<'_>>) {
        let help_content = styled_block(vec![
            vec![
                (
                    Some((ratatui::style::Color::Yellow, ratatui::style::Modifier::BOLD)),
                    "↑/↓, j/k",
                ),
                (None, " - Navigate options / lists"),
            ],
            vec![
                (
                    Some((ratatui::style::Color::Yellow, ratatui::style::Modifier::BOLD)),
                    "Enter, →, l",
                ),
                (None, " - Select option / apply change"),
            ],
            vec![
                (
                    Some((ratatui::style::Color::Yellow, ratatui::style::Modifier::BOLD)),
                    "Tab",
                ),
                (None, " - Switch between password fields"),
            ],
            vec![
                (
                    Some((ratatui::style::Color::Yellow, ratatui::style::Modifier::BOLD)),
                    "Esc, q, ←, h",
                ),
                (None, " - Go back / cancel edit"),
            ],
            vec![(None, "")],
            vec![
                (
                    None,
                    "Modify username, password, shell, sudo (wheel), or delete.",
                ),
            ],
        ]);
        ("Alter User".to_string(), help_content)
    }
}
