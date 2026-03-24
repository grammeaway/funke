use std::io;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState};
use ratatui::Terminal;
use zbus::Connection;

use crate::audio::{self, AudioProfile};
use crate::bluetooth::{self, AdapterInfo, AgentRequest, DeviceInfo};

/// Actions that can result from key handling.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    None,
    ToggleScan,
    ConnectToggle,
    Pair,
    RequestUnpair,
    ConfirmUnpair,
    TrustToggle,
    ConfirmUntrust,
    CancelConfirm,
    AgentSubmit,
    AgentCancel,
    ToggleDetail,
    CloseDetail,
    OpenProfiles,
    SelectProfile,
    CloseProfiles,
    ToggleHelp,
    CloseHelp,
    PowerOn,
    PowerOff,
}

/// An active prompt from the BlueZ pairing agent.
pub enum AgentPrompt {
    PinCode {
        device: String,
        input: String,
        reply: tokio::sync::oneshot::Sender<Option<String>>,
    },
    Passkey {
        device: String,
        input: String,
        reply: tokio::sync::oneshot::Sender<Option<u32>>,
    },
    DisplayPasskey {
        device: String,
        passkey: u32,
    },
    Confirmation {
        device: String,
        passkey: u32,
        reply: tokio::sync::oneshot::Sender<bool>,
    },
    AuthorizeService {
        device: String,
        uuid: String,
        reply: tokio::sync::oneshot::Sender<bool>,
    },
}

/// A profile selection menu for audio devices.
pub struct ProfileMenu {
    pub profiles: Vec<AudioProfile>,
    pub selected: usize,
    pub address: String,
    pub name: String,
}

/// Result of an async device operation.
pub enum DeviceOpResult {
    Connected { address: String, name: String },
    Disconnected { address: String, name: String },
    Paired { address: String, name: String },
    Unpaired { address: String, name: String },
    Trusted { address: String, name: String },
    Untrusted { address: String, name: String },
    ProfilesLoaded { address: String, name: String, profiles: Vec<AudioProfile> },
    ProfileSwitched { name: String, profile: String },
    AdapterFound { adapter: AdapterInfo },
    AdapterPoweredOn,
    AdapterPoweredOff,
    Error { message: String },
}

/// The type of pending confirmation.
#[derive(Debug, Clone, PartialEq)]
pub enum ConfirmType {
    Unpair,
    Untrust,
}

/// A pending confirmation dialog.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingConfirm {
    pub message: String,
    pub address: String,
    pub name: String,
    pub confirm_type: ConfirmType,
}

/// Application state for the TUI.
pub struct App {
    pub adapter: Option<AdapterInfo>,
    pub devices: Vec<DeviceInfo>,
    pub discovered_devices: Vec<DeviceInfo>,
    pub list_state: ListState,
    pub running: bool,
    pub scanning: bool,
    pub status_message: Option<String>,
    pub pending_confirm: Option<PendingConfirm>,
    pub agent_prompt: Option<AgentPrompt>,
    pub show_detail: bool,
    pub profile_menu: Option<ProfileMenu>,
    pub show_help: bool,
}

impl App {
    pub fn new(adapter: Option<AdapterInfo>, devices: Vec<DeviceInfo>) -> Self {
        let mut list_state = ListState::default();
        if !devices.is_empty() {
            list_state.select(Some(0));
        }
        Self {
            adapter,
            devices,
            discovered_devices: Vec::new(),
            list_state,
            running: true,
            scanning: false,
            status_message: None,
            pending_confirm: None,
            agent_prompt: None,
            show_detail: false,
            profile_menu: None,
            show_help: false,
        }
    }

    fn total_devices(&self) -> usize {
        self.devices.len() + self.discovered_devices.len()
    }

    /// Handle a key event. Returns an action for the event loop to process.
    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        // If an agent prompt is active, handle agent-specific input
        if let Some(prompt) = &mut self.agent_prompt {
            return match prompt {
                AgentPrompt::PinCode { input, .. } | AgentPrompt::Passkey { input, .. } => {
                    match key.code {
                        KeyCode::Enter => Action::AgentSubmit,
                        KeyCode::Esc => Action::AgentCancel,
                        KeyCode::Backspace => {
                            input.pop();
                            Action::None
                        }
                        KeyCode::Char(c) => {
                            input.push(c);
                            Action::None
                        }
                        _ => Action::None,
                    }
                }
                AgentPrompt::Confirmation { .. } | AgentPrompt::AuthorizeService { .. } => {
                    match key.code {
                        KeyCode::Char('y') => Action::AgentSubmit,
                        KeyCode::Char('n') | KeyCode::Esc => Action::AgentCancel,
                        _ => Action::None,
                    }
                }
                AgentPrompt::DisplayPasskey { .. } => {
                    // Display-only, dismiss with any key
                    if key.code == KeyCode::Esc || key.code == KeyCode::Enter {
                        Action::AgentCancel
                    } else {
                        Action::None
                    }
                }
            };
        }

        // If profile menu is active, handle profile-specific input
        if let Some(menu) = &mut self.profile_menu {
            return match key.code {
                KeyCode::Up => {
                    if menu.selected > 0 {
                        menu.selected -= 1;
                    } else {
                        menu.selected = menu.profiles.len().saturating_sub(1);
                    }
                    Action::None
                }
                KeyCode::Down => {
                    if menu.selected + 1 < menu.profiles.len() {
                        menu.selected += 1;
                    } else {
                        menu.selected = 0;
                    }
                    Action::None
                }
                KeyCode::Enter => Action::SelectProfile,
                KeyCode::Esc => Action::CloseProfiles,
                _ => Action::None,
            };
        }

        // If a confirmation dialog is active, only handle y/n/Esc
        if let Some(confirm) = &self.pending_confirm {
            return match key.code {
                KeyCode::Char('y') => match confirm.confirm_type {
                    ConfirmType::Unpair => Action::ConfirmUnpair,
                    ConfirmType::Untrust => Action::ConfirmUntrust,
                },
                KeyCode::Char('n') | KeyCode::Esc => Action::CancelConfirm,
                _ => Action::None,
            };
        }

        // If detail view is open, only i or Esc closes it
        if self.show_detail {
            return match key.code {
                KeyCode::Char('i') | KeyCode::Esc => Action::CloseDetail,
                _ => Action::None,
            };
        }

        // If help overlay is open, ? or Esc closes it
        if self.show_help {
            return match key.code {
                KeyCode::Char('?') | KeyCode::Esc => Action::CloseHelp,
                _ => Action::None,
            };
        }

        // If no adapter is present, only allow quit and help
        if self.adapter.is_none() {
            return match key.code {
                KeyCode::Char('q') => {
                    self.running = false;
                    Action::None
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.running = false;
                    Action::None
                }
                KeyCode::Char('?') => Action::ToggleHelp,
                _ => Action::None,
            };
        }

        match key.code {
            KeyCode::Char('q') => {
                self.running = false;
                Action::None
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.running = false;
                Action::None
            }
            KeyCode::Char('s') => Action::ToggleScan,
            KeyCode::Char('p') => Action::Pair,
            KeyCode::Char('u') => Action::RequestUnpair,
            KeyCode::Char('t') => Action::TrustToggle,
            KeyCode::Char('i') => Action::ToggleDetail,
            KeyCode::Char('a') => Action::OpenProfiles,
            KeyCode::Char('?') => Action::ToggleHelp,
            KeyCode::Char('o') => {
                if self.adapter.as_ref().is_some_and(|a| a.powered) {
                    Action::PowerOff
                } else {
                    Action::PowerOn
                }
            }
            KeyCode::Enter => Action::ConnectToggle,
            KeyCode::Up => {
                self.select_previous();
                Action::None
            }
            KeyCode::Down => {
                self.select_next();
                Action::None
            }
            _ => Action::None,
        }
    }

    fn select_next(&mut self) {
        let total = self.total_devices();
        if total == 0 {
            return;
        }
        let i = match self.list_state.selected() {
            Some(i) if i >= total - 1 => 0,
            Some(i) => i + 1,
            None => 0,
        };
        self.list_state.select(Some(i));
    }

    fn select_previous(&mut self) {
        let total = self.total_devices();
        if total == 0 {
            return;
        }
        let i = match self.list_state.selected() {
            Some(0) | None => total - 1,
            Some(i) => i - 1,
        };
        self.list_state.select(Some(i));
    }

    /// Add a discovered device, deduplicating against known and already-discovered devices.
    pub fn add_discovered_device(&mut self, device: DeviceInfo) {
        if self.devices.iter().any(|d| d.address == device.address) {
            return;
        }
        if self.discovered_devices.iter().any(|d| d.address == device.address) {
            return;
        }
        self.discovered_devices.push(device);
        // If nothing was selected and this is the first device overall, select it
        if self.list_state.selected().is_none() && self.devices.is_empty() {
            self.list_state.select(Some(0));
        }
    }

    /// Returns the currently selected device, if any.
    pub fn selected_device(&self) -> Option<&DeviceInfo> {
        let idx = self.list_state.selected()?;
        if idx < self.devices.len() {
            Some(&self.devices[idx])
        } else {
            self.discovered_devices.get(idx - self.devices.len())
        }
    }

    /// Update the `connected` status of a device by address.
    pub fn update_device_connected(&mut self, address: &str, connected: bool) {
        for device in &mut self.devices {
            if device.address == address {
                device.connected = connected;
                return;
            }
        }
    }

    /// Update the `paired` status of a device by address.
    /// If the device was in discovered_devices, move it to known devices.
    pub fn update_device_paired(&mut self, address: &str) {
        // Check if it's already a known device
        for device in &mut self.devices {
            if device.address == address {
                device.paired = true;
                return;
            }
        }
        // Move from discovered to known
        if let Some(pos) = self.discovered_devices.iter().position(|d| d.address == address) {
            let mut device = self.discovered_devices.remove(pos);
            device.paired = true;
            self.devices.push(device);
        }
    }

    /// Update the `trusted` status of a device by address.
    pub fn update_device_trusted(&mut self, address: &str, trusted: bool) {
        for device in &mut self.devices {
            if device.address == address {
                device.trusted = trusted;
                return;
            }
        }
    }

    /// Remove a device from the known devices list (after unpairing).
    pub fn remove_known_device(&mut self, address: &str) {
        self.devices.retain(|d| d.address != address);
        // Adjust selection
        let total = self.total_devices();
        if total == 0 {
            self.list_state.select(None);
        } else if let Some(i) = self.list_state.selected()
            && i >= total
        {
            self.list_state.select(Some(total - 1));
        }
    }

    /// Clear all discovered devices and adjust selection if needed.
    pub fn clear_discovered_devices(&mut self) {
        self.discovered_devices.clear();
        let total = self.devices.len();
        if total == 0 {
            self.list_state.select(None);
        } else if let Some(i) = self.list_state.selected()
            && i >= total
        {
            self.list_state.select(Some(total - 1));
        }
    }
}

/// Initialize the terminal for TUI rendering.
pub fn init_terminal() -> io::Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend)
}

/// Restore the terminal to its original state.
pub fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

/// Format a device as a display line.
fn format_device_line(device: &DeviceInfo, discovered: bool) -> String {
    if discovered {
        format!("[~][ ][ ] {}", device.display_name())
    } else {
        let paired_icon = if device.paired { "P" } else { " " };
        let connected_icon = if device.connected { "C" } else { " " };
        let trusted_icon = if device.trusted { "T" } else { " " };
        format!("[{}][{}][{}] {}", paired_icon, connected_icon, trusted_icon, device.display_name())
    }
}

/// Build context-sensitive help bar text based on the selected device's state.
fn help_bar_text(app: &App) -> String {
    if app.adapter.is_none() {
        return "q: Quit | ?: Help | Waiting for adapter...".to_string();
    }

    if let Some(adapter) = &app.adapter
        && !adapter.powered
    {
        return "q: Quit | o: Power On | ?: Help".to_string();
    }

    let mut hints = vec!["q: Quit", "s: Scan", "o: Power Off", "?: Help"];

    if let Some(device) = app.selected_device() {
        if device.paired {
            if device.connected {
                hints.insert(1, "Enter: Disconnect");
                if device.has_audio_profiles() {
                    hints.insert(2, "a: Profiles");
                }
            } else {
                hints.insert(1, "Enter: Connect");
            }
            if device.trusted {
                hints.insert(2, "t: Untrust");
            } else {
                hints.insert(2, "t: Trust");
            }
            hints.insert(2, "u: Unpair");
        } else {
            hints.insert(1, "p: Pair");
        }
        hints.insert(1, "i: Details");
    }

    hints.join(" | ")
}

/// Draw the UI layout.
pub fn draw(frame: &mut ratatui::Frame, app: &mut App) {
    let chunks = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(frame.area());

    // Device list area
    let device_block = Block::default()
        .title(" Devices ")
        .borders(Borders::ALL);

    let has_devices = !app.devices.is_empty() || !app.discovered_devices.is_empty();

    match &app.adapter {
        None => {
            let no_adapter_list = List::new(vec![
                ListItem::new("No Bluetooth adapter found"),
                ListItem::new("Waiting for adapter..."),
            ])
            .block(device_block);
            frame.render_widget(no_adapter_list, chunks[0]);
        }
        Some(adapter) if !adapter.powered => {
            let off_list = List::new(vec![
                ListItem::new("Bluetooth adapter is powered off"),
                ListItem::new("Press 'o' to power on"),
            ])
            .block(device_block);
            frame.render_widget(off_list, chunks[0]);
        }
        _ if !has_devices => {
            let empty_list = List::new(vec![ListItem::new("No devices")])
                .block(device_block);
            frame.render_widget(empty_list, chunks[0]);
        }
        _ => {
            let mut items: Vec<ListItem> = app
                .devices
                .iter()
                .map(|d| ListItem::new(format_device_line(d, false)))
                .collect();

            for d in &app.discovered_devices {
                items.push(
                    ListItem::new(format_device_line(d, true))
                        .style(Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC)),
                );
            }

            let list = List::new(items)
                .block(device_block)
                .highlight_style(
                    Style::default()
                        .bg(Color::Blue)
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("▸ ");
            frame.render_stateful_widget(list, chunks[0], &mut app.list_state);
        }
    }

    // Confirmation dialog overlay
    if let Some(confirm) = &app.pending_confirm {
        use ratatui::layout::{Alignment, Rect};
        let msg = &confirm.message;
        let width = (msg.len() as u16 + 4).min(frame.area().width);
        let height = 3;
        let area = frame.area();
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;
        let popup_area = Rect::new(x, y, width, height);

        let popup = ratatui::widgets::Paragraph::new(Line::from(msg.as_str()))
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Confirm ")
                    .style(Style::default().fg(Color::Yellow)),
            );
        frame.render_widget(ratatui::widgets::Clear, popup_area);
        frame.render_widget(popup, popup_area);
    }

    // Agent prompt overlay
    if let Some(prompt) = &app.agent_prompt {
        use ratatui::layout::{Alignment, Rect};
        let (title, msg) = match prompt {
            AgentPrompt::PinCode { device, input, .. } => {
                (" PIN Entry ".to_string(), format!("Device: {device}\nPIN: {input}_\n[Enter] Submit  [Esc] Cancel"))
            }
            AgentPrompt::Passkey { device, input, .. } => {
                (" Passkey Entry ".to_string(), format!("Device: {device}\nPasskey: {input}_\n[Enter] Submit  [Esc] Cancel"))
            }
            AgentPrompt::DisplayPasskey { device, passkey } => {
                (" Passkey Display ".to_string(), format!("Device: {device}\nPasskey: {passkey:06}\n[Esc] Dismiss"))
            }
            AgentPrompt::Confirmation { device, passkey, .. } => {
                (" Confirm Passkey ".to_string(), format!("Device: {device}\nPasskey: {passkey:06}\nConfirm? (y/n)"))
            }
            AgentPrompt::AuthorizeService { device, uuid, .. } => {
                (" Authorize Service ".to_string(), format!("Device: {device}\nService: {uuid}\nAuthorize? (y/n)"))
            }
        };
        let lines: Vec<&str> = msg.lines().collect();
        let content_width = lines.iter().map(|l| l.len()).max().unwrap_or(0) as u16;
        let width = (content_width + 4).min(frame.area().width);
        let height = (lines.len() as u16 + 2).min(frame.area().height);
        let area = frame.area();
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;
        let popup_area = Rect::new(x, y, width, height);

        let text: Vec<Line> = lines.iter().map(|l| Line::from(*l)).collect();
        let popup = ratatui::widgets::Paragraph::new(text)
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .style(Style::default().fg(Color::Cyan)),
            );
        frame.render_widget(ratatui::widgets::Clear, popup_area);
        frame.render_widget(popup, popup_area);
    }

    // Device detail overlay
    if app.show_detail
        && let Some(device) = app.selected_device().cloned() {
            use ratatui::layout::{Alignment, Rect};

            let name_line = format!("Name:      {}", device.display_name());
            let addr_line = format!("Address:   {}", device.address);
            let paired_line = format!("Paired:    {}", if device.paired { "Yes" } else { "No" });
            let connected_line = format!("Connected: {}", if device.connected { "Yes" } else { "No" });
            let trusted_line = format!("Trusted:   {}", if device.trusted { "Yes" } else { "No" });
            let type_line = format!("Type:      {}", device.icon.as_deref().unwrap_or("unknown"));

            let mut lines: Vec<Line> = vec![
                Line::from(name_line.clone()),
                Line::from(addr_line.clone()),
                Line::from(paired_line.clone()),
                Line::from(connected_line.clone()),
                Line::from(trusted_line.clone()),
                Line::from(type_line.clone()),
                Line::from(""),
                Line::from("Profiles/UUIDs:"),
            ];

            if device.uuids.is_empty() {
                lines.push(Line::from("  (none)"));
            } else {
                for uuid in &device.uuids {
                    lines.push(Line::from(format!("  {uuid}")));
                }
            }

            lines.push(Line::from(""));
            lines.push(Line::from("[Esc/i] Close"));

            let all_content = [
                &name_line, &addr_line, &paired_line, &connected_line,
                &trusted_line, &type_line,
            ];
            let uuid_max = device.uuids.iter().map(|u| u.len() + 2).max().unwrap_or(8);
            let content_width = all_content.iter()
                .map(|l| l.len())
                .chain(std::iter::once(uuid_max))
                .chain(std::iter::once(15)) // "Profiles/UUIDs:"
                .max()
                .unwrap_or(20) as u16;

            let width = (content_width + 4).min(frame.area().width);
            let height = (lines.len() as u16 + 2).min(frame.area().height);
            let area = frame.area();
            let x = area.x + (area.width.saturating_sub(width)) / 2;
            let y = area.y + (area.height.saturating_sub(height)) / 2;
            let popup_area = Rect::new(x, y, width, height);

            let popup = ratatui::widgets::Paragraph::new(lines)
                .alignment(Alignment::Left)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Device Details ")
                        .style(Style::default().fg(Color::Green)),
                );
            frame.render_widget(ratatui::widgets::Clear, popup_area);
            frame.render_widget(popup, popup_area);
    }

    // Profile menu overlay
    if let Some(menu) = &app.profile_menu {
        use ratatui::layout::{Alignment, Rect};

        let title = format!(" {} - Audio Profiles ", menu.name);
        let mut lines: Vec<Line> = Vec::new();

        for (i, profile) in menu.profiles.iter().enumerate() {
            let active = if profile.active { "● " } else { "  " };
            let cursor = if i == menu.selected { "▸ " } else { "  " };
            let text = format!("{cursor}{active}{}", profile.description);
            let style = if i == menu.selected {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            lines.push(Line::styled(text, style));
        }

        lines.push(Line::from(""));
        lines.push(Line::from("[Enter] Select  [Esc] Close"));

        let content_width = lines
            .iter()
            .map(|l| l.width())
            .chain(std::iter::once(title.len()))
            .max()
            .unwrap_or(20) as u16;

        let width = (content_width + 4).min(frame.area().width);
        let height = (lines.len() as u16 + 2).min(frame.area().height);
        let area = frame.area();
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;
        let popup_area = Rect::new(x, y, width, height);

        let popup = ratatui::widgets::Paragraph::new(lines)
            .alignment(Alignment::Left)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .style(Style::default().fg(Color::Magenta)),
            );
        frame.render_widget(ratatui::widgets::Clear, popup_area);
        frame.render_widget(popup, popup_area);
    }

    // Full help overlay
    if app.show_help {
        use ratatui::layout::{Alignment, Rect};

        let help_lines = vec![
            Line::from("Keyboard Shortcuts"),
            Line::from(""),
            Line::from("  q / Ctrl+C   Quit"),
            Line::from("  s            Toggle scan"),
            Line::from("  Enter        Connect / Disconnect"),
            Line::from("  p            Pair device"),
            Line::from("  u            Unpair device"),
            Line::from("  t            Trust / Untrust device"),
            Line::from("  i            Device details"),
            Line::from("  a            Audio profiles"),
            Line::from("  o            Toggle adapter power"),
            Line::from("  Up/Down      Navigate device list"),
            Line::from("  ?            Toggle this help"),
            Line::from("  Esc          Close overlay / Cancel"),
            Line::from(""),
            Line::from("[?/Esc] Close"),
        ];

        let content_width = help_lines
            .iter()
            .map(|l| l.width())
            .max()
            .unwrap_or(20) as u16;
        let width = (content_width + 4).min(frame.area().width);
        let height = (help_lines.len() as u16 + 2).min(frame.area().height);
        let area = frame.area();
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;
        let popup_area = Rect::new(x, y, width, height);

        let popup = ratatui::widgets::Paragraph::new(help_lines)
            .alignment(Alignment::Left)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Help ")
                    .style(Style::default().fg(Color::Cyan)),
            );
        frame.render_widget(ratatui::widgets::Clear, popup_area);
        frame.render_widget(popup, popup_area);
    }

    // Help bar
    let help_text = help_bar_text(app);
    let help_bar = ratatui::widgets::Paragraph::new(Line::from(format!(" {help_text}")))
        .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(help_bar, chunks[1]);

    // Status bar
    let status_text = if let Some(adapter) = &app.adapter {
        let power_state = if adapter.powered { "ON" } else { "OFF" };
        let scan_indicator = if app.scanning { " | Scanning..." } else { "" };
        let msg_indicator = match &app.status_message {
            Some(msg) => format!(" | {msg}"),
            None => String::new(),
        };
        format!(
            " {} ({}) | Power: {}{}{}",
            adapter.name, adapter.address, power_state, scan_indicator, msg_indicator
        )
    } else {
        let msg_indicator = match &app.status_message {
            Some(msg) => format!(" | {msg}"),
            None => String::new(),
        };
        format!(" No adapter{msg_indicator}")
    };
    let status_bar = ratatui::widgets::Paragraph::new(Line::from(status_text))
        .style(Style::default().fg(Color::White).bg(Color::DarkGray));
    frame.render_widget(status_bar, chunks[2]);
}

/// Run the main TUI event loop.
pub async fn run(
    adapter: Option<AdapterInfo>,
    devices: Vec<DeviceInfo>,
    connection: Connection,
    mut agent_rx: tokio::sync::mpsc::UnboundedReceiver<AgentRequest>,
    agent_tx: tokio::sync::mpsc::UnboundedSender<AgentRequest>,
    mut agent_registered: bool,
) -> io::Result<()> {
    let mut terminal = init_terminal()?;
    let mut app = App::new(adapter, devices);
    let mut adapter_retry_counter: u32 = 0;

    // Channel for discovered devices from D-Bus signals
    let (disc_tx, mut disc_rx) = tokio::sync::mpsc::unbounded_channel();

    // Channel for device operation results (connect/disconnect)
    let (op_tx, mut op_rx) = tokio::sync::mpsc::unbounded_channel::<DeviceOpResult>();

    // Spawn a thread for keyboard input (crossterm events are blocking)
    let (key_tx, mut key_rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || loop {
        match event::poll(std::time::Duration::from_millis(50)) {
            Ok(true) => {
                if let Ok(Event::Key(key)) = event::read()
                    && key.kind == crossterm::event::KeyEventKind::Press
                    && key_tx.send(key).is_err()
                {
                    break;
                }
            }
            Ok(false) => {}
            Err(_) => break,
        }
    });

    let mut scan_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(100));

    loop {
        terminal.draw(|frame| draw(frame, &mut app))?;

        tokio::select! {
            Some(key) = key_rx.recv() => {
                match app.handle_key(key) {
                    Action::ToggleScan => {
                        if app.scanning {
                            let _ = bluetooth::stop_discovery(&connection).await;
                            if let Some(task) = scan_task.take() {
                                task.abort();
                            }
                            app.scanning = false;
                            app.clear_discovered_devices();
                        } else if bluetooth::start_discovery(&connection).await.is_ok() {
                            app.scanning = true;
                            let conn = connection.clone();
                            let tx = disc_tx.clone();
                            scan_task = Some(tokio::spawn(async move {
                                let _ = bluetooth::watch_device_discoveries(&conn, tx).await;
                            }));
                        }
                    }
                    Action::ConnectToggle => {
                        if let Some(device) = app.selected_device() {
                            if !device.paired {
                                app.status_message = Some("Device not paired".to_string());
                            } else {
                                let address = device.address.clone();
                                let name = device.display_name().to_string();
                                let is_connected = device.connected;
                                let conn = connection.clone();
                                let tx = op_tx.clone();
                                tokio::spawn(async move {
                                    if is_connected {
                                        match bluetooth::disconnect_device(&conn, &address).await {
                                            Ok(()) => {
                                                let _ = tx.send(DeviceOpResult::Disconnected { address, name });
                                            }
                                            Err(e) => {
                                                let _ = tx.send(DeviceOpResult::Error {
                                                    message: format!("Disconnect failed: {e}"),
                                                });
                                            }
                                        }
                                    } else {
                                        match bluetooth::connect_device(&conn, &address).await {
                                            Ok(()) => {
                                                let _ = tx.send(DeviceOpResult::Connected { address, name });
                                            }
                                            Err(e) => {
                                                let _ = tx.send(DeviceOpResult::Error {
                                                    message: format!("Connection failed: {e}"),
                                                });
                                            }
                                        }
                                    }
                                });
                            }
                        }
                    }
                    Action::Pair => {
                        if let Some(device) = app.selected_device() {
                            if device.paired {
                                app.status_message = Some("Device already paired".to_string());
                            } else {
                                let address = device.address.clone();
                                let name = device.display_name().to_string();
                                let conn = connection.clone();
                                let tx = op_tx.clone();
                                tokio::spawn(async move {
                                    match bluetooth::pair_device(&conn, &address).await {
                                        Ok(()) => {
                                            let _ = tx.send(DeviceOpResult::Paired { address, name });
                                        }
                                        Err(e) => {
                                            let _ = tx.send(DeviceOpResult::Error {
                                                message: format!("Pairing failed: {e}"),
                                            });
                                        }
                                    }
                                });
                            }
                        }
                    }
                    Action::RequestUnpair => {
                        if let Some(device) = app.selected_device() {
                            if !device.paired {
                                app.status_message = Some("Device not paired".to_string());
                            } else {
                                let name = device.display_name().to_string();
                                let address = device.address.clone();
                                app.pending_confirm = Some(PendingConfirm {
                                    message: format!("Unpair device {name}? y/n"),
                                    address,
                                    name,
                                    confirm_type: ConfirmType::Unpair,
                                });
                            }
                        }
                    }
                    Action::TrustToggle => {
                        if let Some(device) = app.selected_device() {
                            if !device.paired {
                                app.status_message = Some("Device not paired".to_string());
                            } else if device.trusted {
                                // Untrust requires confirmation
                                let name = device.display_name().to_string();
                                let address = device.address.clone();
                                app.pending_confirm = Some(PendingConfirm {
                                    message: format!("Untrust device {name}? y/n"),
                                    address,
                                    name,
                                    confirm_type: ConfirmType::Untrust,
                                });
                            } else {
                                // Trust directly
                                let address = device.address.clone();
                                let name = device.display_name().to_string();
                                let conn = connection.clone();
                                let tx = op_tx.clone();
                                tokio::spawn(async move {
                                    match bluetooth::set_device_trusted(&conn, &address, true).await {
                                        Ok(()) => {
                                            let _ = tx.send(DeviceOpResult::Trusted { address, name });
                                        }
                                        Err(e) => {
                                            let _ = tx.send(DeviceOpResult::Error {
                                                message: format!("Trust failed: {e}"),
                                            });
                                        }
                                    }
                                });
                            }
                        }
                    }
                    Action::ConfirmUntrust => {
                        if let Some(confirm) = app.pending_confirm.take() {
                            let conn = connection.clone();
                            let tx = op_tx.clone();
                            let address = confirm.address;
                            let name = confirm.name;
                            tokio::spawn(async move {
                                match bluetooth::set_device_trusted(&conn, &address, false).await {
                                    Ok(()) => {
                                        let _ = tx.send(DeviceOpResult::Untrusted { address, name });
                                    }
                                    Err(e) => {
                                        let _ = tx.send(DeviceOpResult::Error {
                                            message: format!("Untrust failed: {e}"),
                                        });
                                    }
                                }
                            });
                        }
                    }
                    Action::ConfirmUnpair => {
                        if let Some(confirm) = app.pending_confirm.take() {
                            let conn = connection.clone();
                            let tx = op_tx.clone();
                            let address = confirm.address;
                            let name = confirm.name;
                            tokio::spawn(async move {
                                match bluetooth::remove_device(&conn, &address).await {
                                    Ok(()) => {
                                        let _ = tx.send(DeviceOpResult::Unpaired { address, name });
                                    }
                                    Err(e) => {
                                        let _ = tx.send(DeviceOpResult::Error {
                                            message: format!("Unpair failed: {e}"),
                                        });
                                    }
                                }
                            });
                        }
                    }
                    Action::CancelConfirm => {
                        app.pending_confirm = None;
                    }
                    Action::ToggleDetail => {
                        if app.selected_device().is_some() {
                            app.show_detail = true;
                        }
                    }
                    Action::CloseDetail => {
                        app.show_detail = false;
                    }
                    Action::OpenProfiles => {
                        if let Some(device) = app.selected_device() {
                            if !device.connected {
                                app.status_message = Some("Device not connected".to_string());
                            } else if !device.has_audio_profiles() {
                                app.status_message = Some("No audio profiles available".to_string());
                            } else {
                                let address = device.address.clone();
                                let name = device.display_name().to_string();
                                let tx = op_tx.clone();
                                tokio::spawn(async move {
                                    match audio::get_device_profiles(&address).await {
                                        Ok(profiles) => {
                                            let _ = tx.send(DeviceOpResult::ProfilesLoaded {
                                                address,
                                                name,
                                                profiles,
                                            });
                                        }
                                        Err(e) => {
                                            let _ = tx.send(DeviceOpResult::Error { message: e });
                                        }
                                    }
                                });
                            }
                        }
                    }
                    Action::SelectProfile => {
                        if let Some(menu) = app.profile_menu.take()
                            && let Some(profile) = menu.profiles.get(menu.selected)
                        {
                            let profile_name = profile.name.clone();
                            let profile_desc = profile.description.clone();
                            let address = menu.address;
                            let name = menu.name;
                            let tx = op_tx.clone();
                            tokio::spawn(async move {
                                match audio::set_card_profile(&address, &profile_name).await {
                                    Ok(()) => {
                                        let _ = tx.send(DeviceOpResult::ProfileSwitched {
                                            name,
                                            profile: profile_desc,
                                        });
                                    }
                                    Err(e) => {
                                        let _ = tx.send(DeviceOpResult::Error { message: e });
                                    }
                                }
                            });
                        }
                    }
                    Action::CloseProfiles => {
                        app.profile_menu = None;
                    }
                    Action::ToggleHelp => {
                        app.show_help = true;
                    }
                    Action::CloseHelp => {
                        app.show_help = false;
                    }
                    Action::PowerOn => {
                        let conn = connection.clone();
                        let tx = op_tx.clone();
                        tokio::spawn(async move {
                            match bluetooth::power_on_adapter(&conn).await {
                                Ok(()) => {
                                    let _ = tx.send(DeviceOpResult::AdapterPoweredOn);
                                }
                                Err(e) => {
                                    let _ = tx.send(DeviceOpResult::Error {
                                        message: format!("Power on failed: {e}"),
                                    });
                                }
                            }
                        });
                    }
                    Action::PowerOff => {
                        let conn = connection.clone();
                        let tx = op_tx.clone();
                        tokio::spawn(async move {
                            match bluetooth::power_off_adapter(&conn).await {
                                Ok(()) => {
                                    let _ = tx.send(DeviceOpResult::AdapterPoweredOff);
                                }
                                Err(e) => {
                                    let _ = tx.send(DeviceOpResult::Error {
                                        message: format!("Power off failed: {e}"),
                                    });
                                }
                            }
                        });
                    }
                    Action::AgentSubmit => {
                        if let Some(prompt) = app.agent_prompt.take() {
                            match prompt {
                                AgentPrompt::PinCode { input, reply, .. } => {
                                    let pin = if input.is_empty() { None } else { Some(input) };
                                    let _ = reply.send(pin);
                                }
                                AgentPrompt::Passkey { input, reply, .. } => {
                                    let passkey = input.parse::<u32>().ok();
                                    let _ = reply.send(passkey);
                                }
                                AgentPrompt::Confirmation { reply, .. } => {
                                    let _ = reply.send(true);
                                }
                                AgentPrompt::AuthorizeService { reply, .. } => {
                                    let _ = reply.send(true);
                                }
                                AgentPrompt::DisplayPasskey { .. } => {}
                            }
                        }
                    }
                    Action::AgentCancel => {
                        if let Some(prompt) = app.agent_prompt.take() {
                            match prompt {
                                AgentPrompt::PinCode { reply, .. } => {
                                    let _ = reply.send(None);
                                }
                                AgentPrompt::Passkey { reply, .. } => {
                                    let _ = reply.send(None);
                                }
                                AgentPrompt::Confirmation { reply, .. } => {
                                    let _ = reply.send(false);
                                }
                                AgentPrompt::AuthorizeService { reply, .. } => {
                                    let _ = reply.send(false);
                                }
                                AgentPrompt::DisplayPasskey { .. } => {}
                            }
                        }
                    }
                    Action::None => {}
                }
            }
            Some(result) = op_rx.recv() => {
                match result {
                    DeviceOpResult::Connected { address, name } => {
                        app.update_device_connected(&address, true);
                        app.status_message = Some(format!("Connected to {name}"));
                    }
                    DeviceOpResult::Disconnected { address, name } => {
                        app.update_device_connected(&address, false);
                        app.status_message = Some(format!("Disconnected from {name}"));
                    }
                    DeviceOpResult::Paired { address, name } => {
                        app.update_device_paired(&address);
                        app.status_message = Some(format!("Paired with {name}"));
                    }
                    DeviceOpResult::Unpaired { address, name } => {
                        app.remove_known_device(&address);
                        app.status_message = Some(format!("Unpaired {name}"));
                    }
                    DeviceOpResult::Trusted { address, name } => {
                        app.update_device_trusted(&address, true);
                        app.status_message = Some(format!("Trusted {name}"));
                    }
                    DeviceOpResult::Untrusted { address, name } => {
                        app.update_device_trusted(&address, false);
                        app.status_message = Some(format!("Untrusted {name}"));
                    }
                    DeviceOpResult::ProfilesLoaded { address, name, profiles } => {
                        let selected = profiles.iter().position(|p| p.active).unwrap_or(0);
                        app.profile_menu = Some(ProfileMenu {
                            profiles,
                            selected,
                            address,
                            name,
                        });
                    }
                    DeviceOpResult::ProfileSwitched { name, profile } => {
                        app.status_message = Some(format!("Switched {name} to {profile}"));
                    }
                    DeviceOpResult::AdapterFound { adapter } => {
                        app.adapter = Some(adapter);
                        // Fetch known devices now that adapter is available
                        if let Ok(devs) = bluetooth::get_known_devices(&connection).await {
                            app.devices = devs;
                            if !app.devices.is_empty() && app.list_state.selected().is_none() {
                                app.list_state.select(Some(0));
                            }
                        }
                        // Register the pairing agent if not already registered
                        if !agent_registered
                            && bluetooth::register_agent(&connection, agent_tx.clone()).await.is_ok()
                        {
                            agent_registered = true;
                        }
                        app.status_message = Some("Adapter found".to_string());
                    }
                    DeviceOpResult::AdapterPoweredOn => {
                        if let Some(adapter) = &mut app.adapter {
                            adapter.powered = true;
                        }
                        // Refresh device list now that adapter is on
                        if let Ok(devs) = bluetooth::get_known_devices(&connection).await {
                            app.devices = devs;
                            if !app.devices.is_empty() && app.list_state.selected().is_none() {
                                app.list_state.select(Some(0));
                            }
                        }
                        app.status_message = Some("Adapter powered on".to_string());
                    }
                    DeviceOpResult::AdapterPoweredOff => {
                        if let Some(adapter) = &mut app.adapter {
                            adapter.powered = false;
                        }
                        app.scanning = false;
                        app.devices.clear();
                        app.discovered_devices.clear();
                        app.list_state.select(None);
                        app.status_message = Some("Adapter powered off".to_string());
                    }
                    DeviceOpResult::Error { message } => {
                        app.status_message = Some(message);
                    }
                }
            }
            Some(device) = disc_rx.recv() => {
                app.add_discovered_device(device);
            }
            Some(request) = agent_rx.recv() => {
                match request {
                    AgentRequest::RequestPinCode { device, reply } => {
                        app.agent_prompt = Some(AgentPrompt::PinCode {
                            device,
                            input: String::new(),
                            reply,
                        });
                    }
                    AgentRequest::RequestPasskey { device, reply } => {
                        app.agent_prompt = Some(AgentPrompt::Passkey {
                            device,
                            input: String::new(),
                            reply,
                        });
                    }
                    AgentRequest::DisplayPasskey { device, passkey } => {
                        app.agent_prompt = Some(AgentPrompt::DisplayPasskey {
                            device,
                            passkey,
                        });
                    }
                    AgentRequest::RequestConfirmation { device, passkey, reply } => {
                        app.agent_prompt = Some(AgentPrompt::Confirmation {
                            device,
                            passkey,
                            reply,
                        });
                    }
                    AgentRequest::AuthorizeService { device, uuid, reply } => {
                        app.agent_prompt = Some(AgentPrompt::AuthorizeService {
                            device,
                            uuid,
                            reply,
                        });
                    }
                    AgentRequest::Cancel => {
                        // Cancel any active agent prompt
                        if let Some(prompt) = app.agent_prompt.take() {
                            match prompt {
                                AgentPrompt::PinCode { reply, .. } => { let _ = reply.send(None); }
                                AgentPrompt::Passkey { reply, .. } => { let _ = reply.send(None); }
                                AgentPrompt::Confirmation { reply, .. } => { let _ = reply.send(false); }
                                AgentPrompt::AuthorizeService { reply, .. } => { let _ = reply.send(false); }
                                AgentPrompt::DisplayPasskey { .. } => {}
                            }
                        }
                        app.status_message = Some("Pairing cancelled".to_string());
                    }
                }
            }
            _ = tick.tick() => {
                // Periodically retry adapter detection when no adapter is present
                if app.adapter.is_none() {
                    adapter_retry_counter += 1;
                    if adapter_retry_counter >= 20 {
                        adapter_retry_counter = 0;
                        if let Ok(Some(adapter)) = bluetooth::try_get_adapter_info(&connection).await {
                            let _ = op_tx.send(DeviceOpResult::AdapterFound { adapter });
                        }
                    }
                }
            }
        }

        if !app.running {
            if app.scanning {
                let _ = bluetooth::stop_discovery(&connection).await;
                if let Some(task) = scan_task.take() {
                    task.abort();
                }
            }
            break;
        }
    }

    restore_terminal(&mut terminal)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState};

    fn test_adapter() -> Option<AdapterInfo> {
        Some(AdapterInfo {
            name: "hci0".to_string(),
            address: "AA:BB:CC:DD:EE:FF".to_string(),
            powered: true,
        })
    }

    fn test_devices() -> Vec<DeviceInfo> {
        vec![
            DeviceInfo {
                name: Some("Speaker".to_string()),
                address: "11:22:33:44:55:66".to_string(),
                paired: true,
                connected: true,
                trusted: false,
                icon: Some("audio-card".to_string()),
                uuids: vec!["0000110b-0000-1000-8000-00805f9b34fb".to_string()],
            },
            DeviceInfo {
                name: Some("Keyboard".to_string()),
                address: "AA:BB:CC:DD:EE:FF".to_string(),
                paired: true,
                connected: false,
                trusted: true,
                icon: Some("input-keyboard".to_string()),
                uuids: vec![],
            },
            DeviceInfo {
                name: None,
                address: "FF:EE:DD:CC:BB:AA".to_string(),
                paired: false,
                connected: false,
                trusted: false,
                icon: None,
                uuids: vec![],
            },
        ]
    }

    fn make_key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn buffer_content(backend: &ratatui::backend::TestBackend) -> String {
        backend
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol().chars().next().unwrap_or(' '))
            .collect()
    }

    #[test]
    fn test_app_new_with_devices() {
        let app = App::new(test_adapter(), test_devices());
        assert!(app.running);
        assert!(!app.scanning);
        assert!(app.discovered_devices.is_empty());
        assert_eq!(app.adapter.as_ref().unwrap().name, "hci0");
        assert_eq!(app.devices.len(), 3);
        assert_eq!(app.list_state.selected(), Some(0));
    }

    #[test]
    fn test_app_new_empty_devices() {
        let app = App::new(test_adapter(), vec![]);
        assert_eq!(app.list_state.selected(), None);
    }

    #[test]
    fn test_quit_with_q() {
        let mut app = App::new(test_adapter(), vec![]);
        let action = app.handle_key(make_key(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(!app.running);
        assert_eq!(action, Action::None);
    }

    #[test]
    fn test_quit_with_ctrl_c() {
        let mut app = App::new(test_adapter(), vec![]);
        let action = app.handle_key(make_key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!app.running);
        assert_eq!(action, Action::None);
    }

    #[test]
    fn test_other_keys_dont_quit() {
        let mut app = App::new(test_adapter(), vec![]);
        app.handle_key(make_key(KeyCode::Char('a'), KeyModifiers::NONE));
        assert!(app.running);
    }

    #[test]
    fn test_s_returns_toggle_scan() {
        let mut app = App::new(test_adapter(), vec![]);
        let action = app.handle_key(make_key(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(action, Action::ToggleScan);
        assert!(app.running); // s does not quit
    }

    #[test]
    fn test_enter_returns_connect_toggle() {
        let mut app = App::new(test_adapter(), test_devices());
        let action = app.handle_key(make_key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(action, Action::ConnectToggle);
        assert!(app.running);
    }

    #[test]
    fn test_selected_device() {
        let app = App::new(test_adapter(), test_devices());
        let device = app.selected_device().unwrap();
        assert_eq!(device.address, "11:22:33:44:55:66");
    }

    #[test]
    fn test_selected_device_discovered() {
        let mut app = App::new(test_adapter(), test_devices());
        app.add_discovered_device(DeviceInfo {
            name: Some("New Device".to_string()),
            address: "DD:DD:DD:DD:DD:DD".to_string(),
            paired: false,
            connected: false,
            trusted: false,
            icon: None,
            uuids: vec![],
        });
        app.list_state.select(Some(3));
        let device = app.selected_device().unwrap();
        assert_eq!(device.address, "DD:DD:DD:DD:DD:DD");
    }

    #[test]
    fn test_selected_device_none_when_empty() {
        let app = App::new(test_adapter(), vec![]);
        assert!(app.selected_device().is_none());
    }

    #[test]
    fn test_update_device_connected() {
        let mut app = App::new(test_adapter(), test_devices());
        // Keyboard is at index 1, paired but not connected
        assert!(!app.devices[1].connected);
        app.update_device_connected("AA:BB:CC:DD:EE:FF", true);
        assert!(app.devices[1].connected);
    }

    #[test]
    fn test_update_device_disconnected() {
        let mut app = App::new(test_adapter(), test_devices());
        // Speaker is at index 0, connected
        assert!(app.devices[0].connected);
        app.update_device_connected("11:22:33:44:55:66", false);
        assert!(!app.devices[0].connected);
    }

    #[test]
    fn test_update_device_connected_unknown_address() {
        let mut app = App::new(test_adapter(), test_devices());
        // Should not panic for unknown address
        app.update_device_connected("00:00:00:00:00:00", true);
        // No device changed
        assert!(app.devices[0].connected);
        assert!(!app.devices[1].connected);
    }

    #[test]
    fn test_status_message_initially_none() {
        let app = App::new(test_adapter(), test_devices());
        assert!(app.status_message.is_none());
    }

    #[test]
    fn test_navigate_down() {
        let mut app = App::new(test_adapter(), test_devices());
        assert_eq!(app.list_state.selected(), Some(0));
        app.handle_key(make_key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.list_state.selected(), Some(1));
        app.handle_key(make_key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.list_state.selected(), Some(2));
        // Wraps around
        app.handle_key(make_key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.list_state.selected(), Some(0));
    }

    #[test]
    fn test_navigate_up() {
        let mut app = App::new(test_adapter(), test_devices());
        assert_eq!(app.list_state.selected(), Some(0));
        // Wraps to last
        app.handle_key(make_key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.list_state.selected(), Some(2));
        app.handle_key(make_key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.list_state.selected(), Some(1));
    }

    #[test]
    fn test_navigate_empty_list() {
        let mut app = App::new(test_adapter(), vec![]);
        app.handle_key(make_key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.list_state.selected(), None);
        app.handle_key(make_key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.list_state.selected(), None);
    }

    #[test]
    fn test_navigate_includes_discovered_devices() {
        let mut app = App::new(test_adapter(), test_devices());
        app.add_discovered_device(DeviceInfo {
            name: Some("New Device".to_string()),
            address: "DD:DD:DD:DD:DD:DD".to_string(),
            paired: false,
            connected: false,
            trusted: false,
            icon: None,
            uuids: vec![],
        });
        assert_eq!(app.total_devices(), 4);

        // Navigate to the discovered device (index 3)
        app.list_state.select(Some(2));
        app.handle_key(make_key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.list_state.selected(), Some(3));

        // Wraps around from discovered device
        app.handle_key(make_key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.list_state.selected(), Some(0));
    }

    #[test]
    fn test_add_discovered_device() {
        let mut app = App::new(test_adapter(), test_devices());
        app.add_discovered_device(DeviceInfo {
            name: Some("New Device".to_string()),
            address: "DD:DD:DD:DD:DD:DD".to_string(),
            paired: false,
            connected: false,
            trusted: false,
            icon: None,
            uuids: vec![],
        });
        assert_eq!(app.discovered_devices.len(), 1);
        assert_eq!(app.discovered_devices[0].display_name(), "New Device");
    }

    #[test]
    fn test_add_discovered_device_dedup_known() {
        let mut app = App::new(test_adapter(), test_devices());
        // Try to add a device that already exists in known devices
        app.add_discovered_device(DeviceInfo {
            name: Some("Speaker".to_string()),
            address: "11:22:33:44:55:66".to_string(),
            paired: false,
            connected: false,
            trusted: false,
            icon: None,
            uuids: vec![],
        });
        assert!(app.discovered_devices.is_empty());
    }

    #[test]
    fn test_add_discovered_device_dedup_discovered() {
        let mut app = App::new(test_adapter(), test_devices());
        let device = DeviceInfo {
            name: Some("New Device".to_string()),
            address: "DD:DD:DD:DD:DD:DD".to_string(),
            paired: false,
            connected: false,
            trusted: false,
            icon: None,
            uuids: vec![],
        };
        app.add_discovered_device(device.clone());
        app.add_discovered_device(device);
        assert_eq!(app.discovered_devices.len(), 1);
    }

    #[test]
    fn test_add_discovered_device_selects_first_when_empty() {
        let mut app = App::new(test_adapter(), vec![]);
        assert_eq!(app.list_state.selected(), None);
        app.add_discovered_device(DeviceInfo {
            name: Some("New Device".to_string()),
            address: "DD:DD:DD:DD:DD:DD".to_string(),
            paired: false,
            connected: false,
            trusted: false,
            icon: None,
            uuids: vec![],
        });
        assert_eq!(app.list_state.selected(), Some(0));
    }

    #[test]
    fn test_clear_discovered_devices() {
        let mut app = App::new(test_adapter(), test_devices());
        app.add_discovered_device(DeviceInfo {
            name: Some("New Device".to_string()),
            address: "DD:DD:DD:DD:DD:DD".to_string(),
            paired: false,
            connected: false,
            trusted: false,
            icon: None,
            uuids: vec![],
        });
        assert_eq!(app.total_devices(), 4);

        app.clear_discovered_devices();
        assert!(app.discovered_devices.is_empty());
        assert_eq!(app.total_devices(), 3);
    }

    #[test]
    fn test_clear_discovered_devices_adjusts_selection() {
        let mut app = App::new(test_adapter(), test_devices());
        app.add_discovered_device(DeviceInfo {
            name: Some("New Device".to_string()),
            address: "DD:DD:DD:DD:DD:DD".to_string(),
            paired: false,
            connected: false,
            trusted: false,
            icon: None,
            uuids: vec![],
        });
        // Select the discovered device (index 3)
        app.list_state.select(Some(3));

        app.clear_discovered_devices();
        // Selection should clamp to last known device
        assert_eq!(app.list_state.selected(), Some(2));
    }

    #[test]
    fn test_clear_discovered_devices_empty_known() {
        let mut app = App::new(test_adapter(), vec![]);
        app.add_discovered_device(DeviceInfo {
            name: Some("New Device".to_string()),
            address: "DD:DD:DD:DD:DD:DD".to_string(),
            paired: false,
            connected: false,
            trusted: false,
            icon: None,
            uuids: vec![],
        });
        assert_eq!(app.list_state.selected(), Some(0));

        app.clear_discovered_devices();
        assert_eq!(app.list_state.selected(), None);
    }

    #[test]
    fn test_draw_with_devices() {
        let mut app = App::new(test_adapter(), test_devices());
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let content = buffer_content(terminal.backend());
        assert!(content.contains("Speaker"));
        assert!(content.contains("Keyboard"));
        assert!(content.contains("[P][C][ ] Speaker"));
        assert!(content.contains("[P][ ][T] Keyboard"));
        assert!(content.contains("[ ][ ][ ] FF:EE:DD:CC:BB:AA"));
        assert!(content.contains("Power: ON"));
    }

    #[test]
    fn test_draw_empty_devices() {
        let mut app = App::new(test_adapter(), vec![]);
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let content = buffer_content(terminal.backend());
        assert!(content.contains("No devices"));
    }

    #[test]
    fn test_draw_powered_off() {
        let mut adapter = test_adapter();
        adapter.as_mut().unwrap().powered = false;
        let mut app = App::new(adapter, vec![]);
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let content = buffer_content(terminal.backend());
        assert!(content.contains("Power: OFF"));
    }

    #[test]
    fn test_draw_scanning_indicator() {
        let mut app = App::new(test_adapter(), vec![]);
        app.scanning = true;
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let content = buffer_content(terminal.backend());
        assert!(content.contains("Scanning..."));
    }

    #[test]
    fn test_draw_no_scanning_indicator_when_not_scanning() {
        let mut app = App::new(test_adapter(), vec![]);
        app.scanning = false;
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let content = buffer_content(terminal.backend());
        assert!(!content.contains("Scanning"));
    }

    #[test]
    fn test_draw_status_message() {
        let mut app = App::new(test_adapter(), test_devices());
        app.status_message = Some("Connected to Speaker".to_string());
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let content = buffer_content(terminal.backend());
        assert!(content.contains("Connected to Speaker"));
    }

    #[test]
    fn test_draw_no_status_message() {
        let mut app = App::new(test_adapter(), test_devices());
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let content = buffer_content(terminal.backend());
        // Should not contain a pipe separator for status message
        // Status line is: " hci0 (AA:BB:CC:DD:EE:FF) | Power: ON"
        assert!(!content.contains("Connected to"));
        assert!(!content.contains("Disconnected from"));
    }

    #[test]
    fn test_draw_discovered_devices_distinguishable() {
        let mut app = App::new(test_adapter(), test_devices());
        app.add_discovered_device(DeviceInfo {
            name: Some("New Headset".to_string()),
            address: "DD:DD:DD:DD:DD:DD".to_string(),
            paired: false,
            connected: false,
            trusted: false,
            icon: None,
            uuids: vec![],
        });

        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let content = buffer_content(terminal.backend());
        // Known devices use [P][C] or [ ][ ] format
        assert!(content.contains("[P][C][ ] Speaker"));
        // Discovered devices use [~][ ][ ] format
        assert!(content.contains("[~][ ][ ] New Headset"));
    }

    #[test]
    fn test_format_device_line() {
        let device = DeviceInfo {
            name: Some("Test".to_string()),
            address: "AA:BB:CC:DD:EE:FF".to_string(),
            paired: true,
            connected: true,
            trusted: false,
            icon: None,
            uuids: vec![],
        };
        assert_eq!(format_device_line(&device, false), "[P][C][ ] Test");

        let device2 = DeviceInfo {
            name: None,
            address: "11:22:33:44:55:66".to_string(),
            paired: false,
            connected: false,
            trusted: false,
            icon: None,
            uuids: vec![],
        };
        assert_eq!(format_device_line(&device2, false), "[ ][ ][ ] 11:22:33:44:55:66");
    }

    #[test]
    fn test_format_device_line_trusted() {
        let device = DeviceInfo {
            name: Some("Trusted Device".to_string()),
            address: "AA:BB:CC:DD:EE:FF".to_string(),
            paired: true,
            connected: false,
            trusted: true,
            icon: None,
            uuids: vec![],
        };
        assert_eq!(format_device_line(&device, false), "[P][ ][T] Trusted Device");
    }

    #[test]
    fn test_format_device_line_discovered() {
        let device = DeviceInfo {
            name: Some("New Device".to_string()),
            address: "DD:DD:DD:DD:DD:DD".to_string(),
            paired: false,
            connected: false,
            trusted: false,
            icon: None,
            uuids: vec![],
        };
        assert_eq!(format_device_line(&device, true), "[~][ ][ ] New Device");
    }

    #[test]
    fn test_p_returns_pair_action() {
        let mut app = App::new(test_adapter(), test_devices());
        let action = app.handle_key(make_key(KeyCode::Char('p'), KeyModifiers::NONE));
        assert_eq!(action, Action::Pair);
        assert!(app.running);
    }

    #[test]
    fn test_u_returns_request_unpair_action() {
        let mut app = App::new(test_adapter(), test_devices());
        let action = app.handle_key(make_key(KeyCode::Char('u'), KeyModifiers::NONE));
        assert_eq!(action, Action::RequestUnpair);
    }

    #[test]
    fn test_confirm_dialog_y_returns_confirm_unpair() {
        let mut app = App::new(test_adapter(), test_devices());
        app.pending_confirm = Some(PendingConfirm {
            message: "Unpair device Speaker? y/n".to_string(),
            address: "11:22:33:44:55:66".to_string(),
            name: "Speaker".to_string(),
            confirm_type: ConfirmType::Unpair,
        });
        let action = app.handle_key(make_key(KeyCode::Char('y'), KeyModifiers::NONE));
        assert_eq!(action, Action::ConfirmUnpair);
    }

    #[test]
    fn test_confirm_dialog_n_returns_cancel() {
        let mut app = App::new(test_adapter(), test_devices());
        app.pending_confirm = Some(PendingConfirm {
            message: "Unpair device Speaker? y/n".to_string(),
            address: "11:22:33:44:55:66".to_string(),
            name: "Speaker".to_string(),
            confirm_type: ConfirmType::Unpair,
        });
        let action = app.handle_key(make_key(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(action, Action::CancelConfirm);
    }

    #[test]
    fn test_confirm_dialog_esc_returns_cancel() {
        let mut app = App::new(test_adapter(), test_devices());
        app.pending_confirm = Some(PendingConfirm {
            message: "Unpair device Speaker? y/n".to_string(),
            address: "11:22:33:44:55:66".to_string(),
            name: "Speaker".to_string(),
            confirm_type: ConfirmType::Unpair,
        });
        let action = app.handle_key(make_key(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(action, Action::CancelConfirm);
    }

    #[test]
    fn test_confirm_dialog_blocks_other_keys() {
        let mut app = App::new(test_adapter(), test_devices());
        app.pending_confirm = Some(PendingConfirm {
            message: "Unpair device Speaker? y/n".to_string(),
            address: "11:22:33:44:55:66".to_string(),
            name: "Speaker".to_string(),
            confirm_type: ConfirmType::Unpair,
        });
        // 'q' should not quit while confirm is active
        let action = app.handle_key(make_key(KeyCode::Char('q'), KeyModifiers::NONE));
        assert_eq!(action, Action::None);
        assert!(app.running);
    }

    #[test]
    fn test_update_device_paired_known() {
        let mut app = App::new(test_adapter(), test_devices());
        // Device at index 2 is unpaired
        assert!(!app.devices[2].paired);
        app.update_device_paired("FF:EE:DD:CC:BB:AA");
        assert!(app.devices[2].paired);
    }

    #[test]
    fn test_update_device_paired_moves_discovered() {
        let mut app = App::new(test_adapter(), test_devices());
        app.add_discovered_device(DeviceInfo {
            name: Some("New Device".to_string()),
            address: "DD:DD:DD:DD:DD:DD".to_string(),
            paired: false,
            connected: false,
            trusted: false,
            icon: None,
            uuids: vec![],
        });
        assert_eq!(app.discovered_devices.len(), 1);
        assert_eq!(app.devices.len(), 3);

        app.update_device_paired("DD:DD:DD:DD:DD:DD");

        assert!(app.discovered_devices.is_empty());
        assert_eq!(app.devices.len(), 4);
        assert!(app.devices[3].paired);
    }

    #[test]
    fn test_remove_known_device() {
        let mut app = App::new(test_adapter(), test_devices());
        assert_eq!(app.devices.len(), 3);
        app.remove_known_device("AA:BB:CC:DD:EE:FF");
        assert_eq!(app.devices.len(), 2);
        assert!(!app.devices.iter().any(|d| d.address == "AA:BB:CC:DD:EE:FF"));
    }

    #[test]
    fn test_remove_known_device_adjusts_selection() {
        let mut app = App::new(test_adapter(), test_devices());
        app.list_state.select(Some(2));
        app.remove_known_device("FF:EE:DD:CC:BB:AA"); // remove last
        assert_eq!(app.list_state.selected(), Some(1));
    }

    #[test]
    fn test_remove_known_device_all_empty() {
        let devices = vec![DeviceInfo {
            name: Some("Only".to_string()),
            address: "11:11:11:11:11:11".to_string(),
            paired: true,
            connected: false,
            trusted: false,
            icon: None,
            uuids: vec![],
        }];
        let mut app = App::new(test_adapter(), devices);
        app.remove_known_device("11:11:11:11:11:11");
        assert_eq!(app.list_state.selected(), None);
    }

    #[test]
    fn test_pending_confirm_initially_none() {
        let app = App::new(test_adapter(), test_devices());
        assert!(app.pending_confirm.is_none());
    }

    #[test]
    fn test_agent_prompt_initially_none() {
        let app = App::new(test_adapter(), test_devices());
        assert!(app.agent_prompt.is_none());
    }

    #[test]
    fn test_agent_pin_input_typing() {
        let mut app = App::new(test_adapter(), test_devices());
        let (_tx, _rx) = tokio::sync::oneshot::channel();
        app.agent_prompt = Some(AgentPrompt::PinCode {
            device: "test".to_string(),
            input: String::new(),
            reply: _tx,
        });
        // Type characters
        let action = app.handle_key(make_key(KeyCode::Char('1'), KeyModifiers::NONE));
        assert_eq!(action, Action::None);
        if let Some(AgentPrompt::PinCode { input, .. }) = &app.agent_prompt {
            assert_eq!(input, "1");
        } else {
            panic!("Expected PinCode prompt");
        }

        app.handle_key(make_key(KeyCode::Char('2'), KeyModifiers::NONE));
        app.handle_key(make_key(KeyCode::Char('3'), KeyModifiers::NONE));
        if let Some(AgentPrompt::PinCode { input, .. }) = &app.agent_prompt {
            assert_eq!(input, "123");
        }
    }

    #[test]
    fn test_agent_pin_input_backspace() {
        let mut app = App::new(test_adapter(), test_devices());
        let (tx, _rx) = tokio::sync::oneshot::channel();
        app.agent_prompt = Some(AgentPrompt::PinCode {
            device: "test".to_string(),
            input: "123".to_string(),
            reply: tx,
        });
        app.handle_key(make_key(KeyCode::Backspace, KeyModifiers::NONE));
        if let Some(AgentPrompt::PinCode { input, .. }) = &app.agent_prompt {
            assert_eq!(input, "12");
        }
    }

    #[test]
    fn test_agent_pin_enter_submits() {
        let mut app = App::new(test_adapter(), test_devices());
        let (tx, _rx) = tokio::sync::oneshot::channel();
        app.agent_prompt = Some(AgentPrompt::PinCode {
            device: "test".to_string(),
            input: "1234".to_string(),
            reply: tx,
        });
        let action = app.handle_key(make_key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(action, Action::AgentSubmit);
    }

    #[test]
    fn test_agent_pin_esc_cancels() {
        let mut app = App::new(test_adapter(), test_devices());
        let (tx, _rx) = tokio::sync::oneshot::channel();
        app.agent_prompt = Some(AgentPrompt::PinCode {
            device: "test".to_string(),
            input: String::new(),
            reply: tx,
        });
        let action = app.handle_key(make_key(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(action, Action::AgentCancel);
    }

    #[test]
    fn test_agent_confirmation_y_submits() {
        let mut app = App::new(test_adapter(), test_devices());
        let (tx, _rx) = tokio::sync::oneshot::channel();
        app.agent_prompt = Some(AgentPrompt::Confirmation {
            device: "test".to_string(),
            passkey: 123456,
            reply: tx,
        });
        let action = app.handle_key(make_key(KeyCode::Char('y'), KeyModifiers::NONE));
        assert_eq!(action, Action::AgentSubmit);
    }

    #[test]
    fn test_agent_confirmation_n_cancels() {
        let mut app = App::new(test_adapter(), test_devices());
        let (tx, _rx) = tokio::sync::oneshot::channel();
        app.agent_prompt = Some(AgentPrompt::Confirmation {
            device: "test".to_string(),
            passkey: 123456,
            reply: tx,
        });
        let action = app.handle_key(make_key(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(action, Action::AgentCancel);
    }

    #[test]
    fn test_agent_prompt_blocks_other_keys() {
        let mut app = App::new(test_adapter(), test_devices());
        let (tx, _rx) = tokio::sync::oneshot::channel();
        app.agent_prompt = Some(AgentPrompt::Confirmation {
            device: "test".to_string(),
            passkey: 123456,
            reply: tx,
        });
        // 'q' should not quit while agent prompt is active
        let action = app.handle_key(make_key(KeyCode::Char('q'), KeyModifiers::NONE));
        assert_eq!(action, Action::None);
        assert!(app.running);
    }

    #[test]
    fn test_agent_passkey_input_typing() {
        let mut app = App::new(test_adapter(), test_devices());
        let (tx, _rx) = tokio::sync::oneshot::channel();
        app.agent_prompt = Some(AgentPrompt::Passkey {
            device: "test".to_string(),
            input: String::new(),
            reply: tx,
        });
        app.handle_key(make_key(KeyCode::Char('4'), KeyModifiers::NONE));
        app.handle_key(make_key(KeyCode::Char('2'), KeyModifiers::NONE));
        if let Some(AgentPrompt::Passkey { input, .. }) = &app.agent_prompt {
            assert_eq!(input, "42");
        }
    }

    #[test]
    fn test_agent_display_passkey_dismiss() {
        let mut app = App::new(test_adapter(), test_devices());
        app.agent_prompt = Some(AgentPrompt::DisplayPasskey {
            device: "test".to_string(),
            passkey: 123456,
        });
        let action = app.handle_key(make_key(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(action, Action::AgentCancel);
    }

    #[test]
    fn test_agent_authorize_service_y() {
        let mut app = App::new(test_adapter(), test_devices());
        let (tx, _rx) = tokio::sync::oneshot::channel();
        app.agent_prompt = Some(AgentPrompt::AuthorizeService {
            device: "test".to_string(),
            uuid: "0000110b-0000-1000-8000-00805f9b34fb".to_string(),
            reply: tx,
        });
        let action = app.handle_key(make_key(KeyCode::Char('y'), KeyModifiers::NONE));
        assert_eq!(action, Action::AgentSubmit);
    }

    #[test]
    fn test_draw_agent_pin_prompt() {
        let mut app = App::new(test_adapter(), test_devices());
        let (tx, _rx) = tokio::sync::oneshot::channel();
        app.agent_prompt = Some(AgentPrompt::PinCode {
            device: "/org/bluez/hci0/dev_AA_BB".to_string(),
            input: "12".to_string(),
            reply: tx,
        });
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let content = buffer_content(terminal.backend());
        assert!(content.contains("PIN Entry"));
        assert!(content.contains("12"));
    }

    #[test]
    fn test_draw_agent_confirmation_prompt() {
        let mut app = App::new(test_adapter(), test_devices());
        let (tx, _rx) = tokio::sync::oneshot::channel();
        app.agent_prompt = Some(AgentPrompt::Confirmation {
            device: "/org/bluez/hci0/dev_AA_BB".to_string(),
            passkey: 123456,
            reply: tx,
        });
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let content = buffer_content(terminal.backend());
        assert!(content.contains("Confirm Passkey"));
        assert!(content.contains("123456"));
    }

    #[test]
    fn test_draw_agent_display_passkey() {
        let mut app = App::new(test_adapter(), test_devices());
        app.agent_prompt = Some(AgentPrompt::DisplayPasskey {
            device: "/org/bluez/hci0/dev_AA_BB".to_string(),
            passkey: 42,
        });
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let content = buffer_content(terminal.backend());
        assert!(content.contains("Passkey Display"));
        assert!(content.contains("000042"));
    }

    #[test]
    fn test_draw_confirm_dialog() {
        let mut app = App::new(test_adapter(), test_devices());
        app.pending_confirm = Some(PendingConfirm {
            message: "Unpair device Speaker? y/n".to_string(),
            address: "11:22:33:44:55:66".to_string(),
            name: "Speaker".to_string(),
            confirm_type: ConfirmType::Unpair,
        });
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let content = buffer_content(terminal.backend());
        assert!(content.contains("Unpair device Speaker? y/n"));
        assert!(content.contains("Confirm"));
    }

    #[test]
    fn test_show_detail_initially_false() {
        let app = App::new(test_adapter(), test_devices());
        assert!(!app.show_detail);
    }

    #[test]
    fn test_i_returns_toggle_detail() {
        let mut app = App::new(test_adapter(), test_devices());
        let action = app.handle_key(make_key(KeyCode::Char('i'), KeyModifiers::NONE));
        assert_eq!(action, Action::ToggleDetail);
        assert!(app.running);
    }

    #[test]
    fn test_detail_view_i_closes() {
        let mut app = App::new(test_adapter(), test_devices());
        app.show_detail = true;
        let action = app.handle_key(make_key(KeyCode::Char('i'), KeyModifiers::NONE));
        assert_eq!(action, Action::CloseDetail);
    }

    #[test]
    fn test_detail_view_esc_closes() {
        let mut app = App::new(test_adapter(), test_devices());
        app.show_detail = true;
        let action = app.handle_key(make_key(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(action, Action::CloseDetail);
    }

    #[test]
    fn test_detail_view_blocks_other_keys() {
        let mut app = App::new(test_adapter(), test_devices());
        app.show_detail = true;
        // 'q' should not quit while detail view is open
        let action = app.handle_key(make_key(KeyCode::Char('q'), KeyModifiers::NONE));
        assert_eq!(action, Action::None);
        assert!(app.running);
        // 's' should not toggle scan
        let action = app.handle_key(make_key(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(action, Action::None);
    }

    #[test]
    fn test_draw_detail_view() {
        let mut app = App::new(test_adapter(), test_devices());
        app.show_detail = true;
        let backend = ratatui::backend::TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let content = buffer_content(terminal.backend());
        assert!(content.contains("Device Details"));
        assert!(content.contains("Speaker"));
        assert!(content.contains("11:22:33:44:55:66"));
        assert!(content.contains("Paired"));
        assert!(content.contains("Connected"));
        assert!(content.contains("Trusted"));
        assert!(content.contains("audio-card"));
        assert!(content.contains("Profiles/UUIDs"));
    }

    #[test]
    fn test_draw_detail_view_no_icon() {
        let mut app = App::new(test_adapter(), test_devices());
        // Select device at index 2 (no name, no icon)
        app.list_state.select(Some(2));
        app.show_detail = true;
        let backend = ratatui::backend::TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let content = buffer_content(terminal.backend());
        assert!(content.contains("Device Details"));
        assert!(content.contains("FF:EE:DD:CC:BB:AA"));
        assert!(content.contains("unknown"));
        assert!(content.contains("(none)"));
    }

    #[test]
    fn test_draw_detail_view_with_uuids() {
        let mut app = App::new(test_adapter(), test_devices());
        // Speaker at index 0 has a UUID
        app.list_state.select(Some(0));
        app.show_detail = true;
        let backend = ratatui::backend::TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let content = buffer_content(terminal.backend());
        assert!(content.contains("0000110b-0000-1000-8000-00805f9b34fb"));
    }

    #[test]
    fn test_draw_detail_view_trusted_device() {
        let mut app = App::new(test_adapter(), test_devices());
        // Keyboard at index 1 is trusted
        app.list_state.select(Some(1));
        app.show_detail = true;
        let backend = ratatui::backend::TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let content = buffer_content(terminal.backend());
        assert!(content.contains("Trusted:   Yes"));
    }

    #[test]
    fn test_detail_not_shown_when_no_device_selected() {
        let mut app = App::new(test_adapter(), vec![]);
        app.show_detail = true;
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let content = buffer_content(terminal.backend());
        // No device selected, so detail popup should not appear
        assert!(!content.contains("Device Details"));
    }

    #[test]
    fn test_t_returns_trust_toggle() {
        let mut app = App::new(test_adapter(), test_devices());
        let action = app.handle_key(make_key(KeyCode::Char('t'), KeyModifiers::NONE));
        assert_eq!(action, Action::TrustToggle);
        assert!(app.running);
    }

    #[test]
    fn test_update_device_trusted() {
        let mut app = App::new(test_adapter(), test_devices());
        // Speaker at index 0 is not trusted
        assert!(!app.devices[0].trusted);
        app.update_device_trusted("11:22:33:44:55:66", true);
        assert!(app.devices[0].trusted);
    }

    #[test]
    fn test_update_device_untrusted() {
        let mut app = App::new(test_adapter(), test_devices());
        // Keyboard at index 1 is trusted
        assert!(app.devices[1].trusted);
        app.update_device_trusted("AA:BB:CC:DD:EE:FF", false);
        assert!(!app.devices[1].trusted);
    }

    #[test]
    fn test_update_device_trusted_unknown_address() {
        let mut app = App::new(test_adapter(), test_devices());
        // Should not panic for unknown address
        app.update_device_trusted("00:00:00:00:00:00", true);
        assert!(!app.devices[0].trusted);
        assert!(app.devices[1].trusted);
    }

    #[test]
    fn test_confirm_untrust_dialog_y_returns_confirm_untrust() {
        let mut app = App::new(test_adapter(), test_devices());
        app.pending_confirm = Some(PendingConfirm {
            message: "Untrust device Keyboard? y/n".to_string(),
            address: "AA:BB:CC:DD:EE:FF".to_string(),
            name: "Keyboard".to_string(),
            confirm_type: ConfirmType::Untrust,
        });
        let action = app.handle_key(make_key(KeyCode::Char('y'), KeyModifiers::NONE));
        assert_eq!(action, Action::ConfirmUntrust);
    }

    #[test]
    fn test_confirm_untrust_dialog_n_returns_cancel() {
        let mut app = App::new(test_adapter(), test_devices());
        app.pending_confirm = Some(PendingConfirm {
            message: "Untrust device Keyboard? y/n".to_string(),
            address: "AA:BB:CC:DD:EE:FF".to_string(),
            name: "Keyboard".to_string(),
            confirm_type: ConfirmType::Untrust,
        });
        let action = app.handle_key(make_key(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(action, Action::CancelConfirm);
    }

    #[test]
    fn test_confirm_untrust_dialog_blocks_other_keys() {
        let mut app = App::new(test_adapter(), test_devices());
        app.pending_confirm = Some(PendingConfirm {
            message: "Untrust device Keyboard? y/n".to_string(),
            address: "AA:BB:CC:DD:EE:FF".to_string(),
            name: "Keyboard".to_string(),
            confirm_type: ConfirmType::Untrust,
        });
        let action = app.handle_key(make_key(KeyCode::Char('q'), KeyModifiers::NONE));
        assert_eq!(action, Action::None);
        assert!(app.running);
    }

    #[test]
    fn test_draw_trust_indicator_in_device_list() {
        let mut app = App::new(test_adapter(), test_devices());
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let content = buffer_content(terminal.backend());
        // Keyboard is trusted
        assert!(content.contains("[P][ ][T] Keyboard"));
        // Speaker is not trusted
        assert!(content.contains("[P][C][ ] Speaker"));
    }

    #[test]
    fn test_draw_untrust_confirm_dialog() {
        let mut app = App::new(test_adapter(), test_devices());
        app.pending_confirm = Some(PendingConfirm {
            message: "Untrust device Keyboard? y/n".to_string(),
            address: "AA:BB:CC:DD:EE:FF".to_string(),
            name: "Keyboard".to_string(),
            confirm_type: ConfirmType::Untrust,
        });
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let content = buffer_content(terminal.backend());
        assert!(content.contains("Untrust device Keyboard? y/n"));
        assert!(content.contains("Confirm"));
    }

    #[test]
    fn test_a_returns_open_profiles() {
        let mut app = App::new(test_adapter(), test_devices());
        let action = app.handle_key(make_key(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(action, Action::OpenProfiles);
        assert!(app.running);
    }

    #[test]
    fn test_profile_menu_initially_none() {
        let app = App::new(test_adapter(), test_devices());
        assert!(app.profile_menu.is_none());
    }

    fn test_profile_menu() -> ProfileMenu {
        ProfileMenu {
            profiles: vec![
                AudioProfile {
                    name: "a2dp-sink".to_string(),
                    description: "High Fidelity Playback (A2DP Sink)".to_string(),
                    active: true,
                },
                AudioProfile {
                    name: "headset-head-unit".to_string(),
                    description: "Headset Head Unit (HSP/HFP)".to_string(),
                    active: false,
                },
            ],
            selected: 0,
            address: "11:22:33:44:55:66".to_string(),
            name: "Speaker".to_string(),
        }
    }

    #[test]
    fn test_profile_menu_navigate_down() {
        let mut app = App::new(test_adapter(), test_devices());
        app.profile_menu = Some(test_profile_menu());

        let action = app.handle_key(make_key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(action, Action::None);
        assert_eq!(app.profile_menu.as_ref().unwrap().selected, 1);
    }

    #[test]
    fn test_profile_menu_navigate_down_wraps() {
        let mut app = App::new(test_adapter(), test_devices());
        app.profile_menu = Some(test_profile_menu());
        app.profile_menu.as_mut().unwrap().selected = 1;

        app.handle_key(make_key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.profile_menu.as_ref().unwrap().selected, 0);
    }

    #[test]
    fn test_profile_menu_navigate_up() {
        let mut app = App::new(test_adapter(), test_devices());
        app.profile_menu = Some(test_profile_menu());
        app.profile_menu.as_mut().unwrap().selected = 1;

        let action = app.handle_key(make_key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(action, Action::None);
        assert_eq!(app.profile_menu.as_ref().unwrap().selected, 0);
    }

    #[test]
    fn test_profile_menu_navigate_up_wraps() {
        let mut app = App::new(test_adapter(), test_devices());
        app.profile_menu = Some(test_profile_menu());

        app.handle_key(make_key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.profile_menu.as_ref().unwrap().selected, 1);
    }

    #[test]
    fn test_profile_menu_enter_selects() {
        let mut app = App::new(test_adapter(), test_devices());
        app.profile_menu = Some(test_profile_menu());

        let action = app.handle_key(make_key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(action, Action::SelectProfile);
    }

    #[test]
    fn test_profile_menu_esc_closes() {
        let mut app = App::new(test_adapter(), test_devices());
        app.profile_menu = Some(test_profile_menu());

        let action = app.handle_key(make_key(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(action, Action::CloseProfiles);
    }

    #[test]
    fn test_profile_menu_blocks_other_keys() {
        let mut app = App::new(test_adapter(), test_devices());
        app.profile_menu = Some(test_profile_menu());

        let action = app.handle_key(make_key(KeyCode::Char('q'), KeyModifiers::NONE));
        assert_eq!(action, Action::None);
        assert!(app.running);

        let action = app.handle_key(make_key(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(action, Action::None);
    }

    #[test]
    fn test_draw_profile_menu() {
        let mut app = App::new(test_adapter(), test_devices());
        app.profile_menu = Some(test_profile_menu());

        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let content = buffer_content(terminal.backend());
        assert!(content.contains("Audio Profiles"));
        assert!(content.contains("High Fidelity Playback"));
        assert!(content.contains("Headset Head Unit"));
    }

    #[test]
    fn test_draw_profile_menu_active_indicator() {
        let mut app = App::new(test_adapter(), test_devices());
        app.profile_menu = Some(test_profile_menu());

        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let content = buffer_content(terminal.backend());
        // Active profile should have ● indicator
        assert!(content.contains("●"));
    }

    // --- US-011: Help bar and help overlay tests ---

    #[test]
    fn test_question_mark_returns_toggle_help() {
        let mut app = App::new(test_adapter(), test_devices());
        let action = app.handle_key(make_key(KeyCode::Char('?'), KeyModifiers::NONE));
        assert_eq!(action, Action::ToggleHelp);
    }

    #[test]
    fn test_help_overlay_close_with_question_mark() {
        let mut app = App::new(test_adapter(), test_devices());
        app.show_help = true;
        let action = app.handle_key(make_key(KeyCode::Char('?'), KeyModifiers::NONE));
        assert_eq!(action, Action::CloseHelp);
    }

    #[test]
    fn test_help_overlay_close_with_esc() {
        let mut app = App::new(test_adapter(), test_devices());
        app.show_help = true;
        let action = app.handle_key(make_key(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(action, Action::CloseHelp);
    }

    #[test]
    fn test_help_overlay_blocks_other_keys() {
        let mut app = App::new(test_adapter(), test_devices());
        app.show_help = true;
        let action = app.handle_key(make_key(KeyCode::Char('q'), KeyModifiers::NONE));
        assert_eq!(action, Action::None);
        assert!(app.running); // q should not quit while help is open
    }

    #[test]
    fn test_show_help_initially_false() {
        let app = App::new(test_adapter(), test_devices());
        assert!(!app.show_help);
    }

    #[test]
    fn test_help_bar_no_device_selected() {
        let app = App::new(test_adapter(), vec![]);
        let text = help_bar_text(&app);
        assert!(text.contains("q: Quit"));
        assert!(text.contains("s: Scan"));
        assert!(text.contains("?: Help"));
        assert!(!text.contains("Enter:"));
    }

    #[test]
    fn test_help_bar_paired_connected_device() {
        let app = App::new(test_adapter(), test_devices());
        // First device is paired + connected + has audio UUIDs
        let text = help_bar_text(&app);
        assert!(text.contains("Enter: Disconnect"));
        assert!(text.contains("u: Unpair"));
        assert!(text.contains("a: Profiles"));
        assert!(text.contains("i: Details"));
    }

    #[test]
    fn test_help_bar_paired_disconnected_device() {
        let mut app = App::new(test_adapter(), test_devices());
        app.list_state.select(Some(1)); // Keyboard: paired, not connected
        let text = help_bar_text(&app);
        assert!(text.contains("Enter: Connect"));
        assert!(!text.contains("Enter: Disconnect"));
        assert!(text.contains("u: Unpair"));
    }

    #[test]
    fn test_help_bar_unpaired_device() {
        let mut app = App::new(test_adapter(), test_devices());
        app.list_state.select(Some(2)); // Unpaired device
        let text = help_bar_text(&app);
        assert!(text.contains("p: Pair"));
        assert!(!text.contains("Enter: Connect"));
        assert!(!text.contains("u: Unpair"));
    }

    #[test]
    fn test_help_bar_trusted_device_shows_untrust() {
        let mut app = App::new(test_adapter(), test_devices());
        app.list_state.select(Some(1)); // Keyboard: paired + trusted
        let text = help_bar_text(&app);
        assert!(text.contains("t: Untrust"));
    }

    #[test]
    fn test_help_bar_untrusted_device_shows_trust() {
        let app = App::new(test_adapter(), test_devices());
        // First device (Speaker): paired, not trusted
        let text = help_bar_text(&app);
        assert!(text.contains("t: Trust"));
    }

    #[test]
    fn test_draw_help_bar_visible() {
        let mut app = App::new(test_adapter(), test_devices());
        let backend = ratatui::backend::TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let content = buffer_content(terminal.backend());
        assert!(content.contains("q: Quit"));
        assert!(content.contains("?: Help"));
    }

    #[test]
    fn test_draw_help_overlay() {
        let mut app = App::new(test_adapter(), test_devices());
        app.show_help = true;
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let content = buffer_content(terminal.backend());
        assert!(content.contains("Keyboard Shortcuts"));
        assert!(content.contains("Toggle scan"));
        assert!(content.contains("Audio profiles"));
    }

    #[test]
    fn test_help_bar_no_audio_profiles_for_non_audio_device() {
        let mut app = App::new(test_adapter(), test_devices());
        app.list_state.select(Some(1)); // Keyboard: no audio UUIDs
        let text = help_bar_text(&app);
        assert!(!text.contains("a: Profiles"));
    }

    // --- US-012: Error Handling and Resilience ---

    fn test_adapter_powered_off() -> Option<AdapterInfo> {
        Some(AdapterInfo {
            name: "hci0".to_string(),
            address: "AA:BB:CC:DD:EE:FF".to_string(),
            powered: false,
        })
    }

    #[test]
    fn test_draw_adapter_powered_off_shows_message() {
        let mut app = App::new(test_adapter_powered_off(), vec![]);
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let content = buffer_content(terminal.backend());
        assert!(content.contains("powered off"));
        assert!(content.contains("Power: OFF"));
    }

    #[test]
    fn test_draw_adapter_powered_off_shows_power_on_hint() {
        let mut app = App::new(test_adapter_powered_off(), vec![]);
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let content = buffer_content(terminal.backend());
        assert!(content.contains("'o' to power on"));
    }

    #[test]
    fn test_help_bar_adapter_powered_off() {
        let app = App::new(test_adapter_powered_off(), vec![]);
        let text = help_bar_text(&app);
        assert!(text.contains("o: Power On"));
        assert!(text.contains("q: Quit"));
        assert!(!text.contains("s: Scan"));
    }

    #[test]
    fn test_help_bar_adapter_powered_on() {
        let app = App::new(test_adapter(), test_devices());
        let text = help_bar_text(&app);
        assert!(text.contains("s: Scan"));
        assert!(text.contains("o: Power Off"));
        assert!(!text.contains("o: Power On"));
    }

    #[test]
    fn test_key_o_when_adapter_off_returns_power_on() {
        let mut app = App::new(test_adapter_powered_off(), vec![]);
        let key = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE);
        assert_eq!(app.handle_key(key), Action::PowerOn);
    }

    #[test]
    fn test_key_o_when_adapter_on_returns_power_off() {
        let mut app = App::new(test_adapter(), test_devices());
        let key = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE);
        assert_eq!(app.handle_key(key), Action::PowerOff);
    }

    #[test]
    fn test_adapter_powered_on_updates_state() {
        let mut app = App::new(test_adapter_powered_off(), vec![]);
        assert!(!app.adapter.as_ref().unwrap().powered);
        app.adapter.as_mut().unwrap().powered = true;
        assert!(app.adapter.as_ref().unwrap().powered);
    }

    #[test]
    fn test_draw_adapter_off_hides_devices() {
        // Even if there are devices, powered-off state should show the power-off message
        let mut app = App::new(test_adapter_powered_off(), test_devices());
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let content = buffer_content(terminal.backend());
        assert!(content.contains("powered off"));
    }

    #[test]
    fn test_help_overlay_includes_power_on() {
        let mut app = App::new(test_adapter(), test_devices());
        app.show_help = true;
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let content = buffer_content(terminal.backend());
        assert!(content.contains("Toggle adapter power"));
    }

    #[test]
    fn test_status_bar_shows_power_off() {
        let mut app = App::new(test_adapter_powered_off(), vec![]);
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let content = buffer_content(terminal.backend());
        assert!(content.contains("Power: OFF"));
    }

    #[test]
    fn test_adapter_powered_off_clears_state() {
        let mut app = App::new(test_adapter(), test_devices());
        assert!(app.adapter.as_ref().unwrap().powered);
        assert!(!app.devices.is_empty());

        // Simulate power-off result
        app.adapter.as_mut().unwrap().powered = false;
        app.scanning = false;
        app.devices.clear();
        app.discovered_devices.clear();
        app.list_state.select(None);

        assert!(!app.adapter.as_ref().unwrap().powered);
        assert!(app.devices.is_empty());
        assert!(app.discovered_devices.is_empty());
        assert!(!app.scanning);
        assert_eq!(app.list_state.selected(), None);
    }

    #[test]
    fn test_status_bar_shows_dbus_errors() {
        let mut app = App::new(test_adapter(), test_devices());
        app.status_message = Some("Connection failed: org.bluez.Error".to_string());
        let backend = ratatui::backend::TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let content = buffer_content(terminal.backend());
        assert!(content.contains("Connection failed"));
    }

    // --- No-adapter tests ---

    #[test]
    fn test_app_new_no_adapter() {
        let app = App::new(None, vec![]);
        assert!(app.adapter.is_none());
        assert!(app.devices.is_empty());
        assert!(app.running);
        assert_eq!(app.list_state.selected(), None);
    }

    #[test]
    fn test_draw_no_adapter_shows_message() {
        let mut app = App::new(None, vec![]);
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let content = buffer_content(terminal.backend());
        assert!(content.contains("No Bluetooth adapter found"));
    }

    #[test]
    fn test_draw_no_adapter_shows_waiting() {
        let mut app = App::new(None, vec![]);
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let content = buffer_content(terminal.backend());
        assert!(content.contains("Waiting for adapter"));
    }

    #[test]
    fn test_key_handling_no_adapter_quit() {
        let mut app = App::new(None, vec![]);
        let action = app.handle_key(make_key(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(!app.running);
        assert_eq!(action, Action::None);
    }

    #[test]
    fn test_key_handling_no_adapter_ctrl_c() {
        let mut app = App::new(None, vec![]);
        let action = app.handle_key(make_key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!app.running);
        assert_eq!(action, Action::None);
    }

    #[test]
    fn test_key_handling_no_adapter_help() {
        let mut app = App::new(None, vec![]);
        let action = app.handle_key(make_key(KeyCode::Char('?'), KeyModifiers::NONE));
        assert_eq!(action, Action::ToggleHelp);
    }

    #[test]
    fn test_key_handling_no_adapter_blocks_scan() {
        let mut app = App::new(None, vec![]);
        let action = app.handle_key(make_key(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(action, Action::None);
    }

    #[test]
    fn test_key_handling_no_adapter_blocks_connect() {
        let mut app = App::new(None, vec![]);
        let action = app.handle_key(make_key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(action, Action::None);
    }

    #[test]
    fn test_key_handling_no_adapter_blocks_power() {
        let mut app = App::new(None, vec![]);
        let action = app.handle_key(make_key(KeyCode::Char('o'), KeyModifiers::NONE));
        assert_eq!(action, Action::None);
    }

    #[test]
    fn test_help_bar_no_adapter() {
        let app = App::new(None, vec![]);
        let text = help_bar_text(&app);
        assert!(text.contains("Quit"));
        assert!(text.contains("Help"));
        assert!(text.contains("Waiting for adapter"));
    }

    #[test]
    fn test_status_bar_no_adapter() {
        let mut app = App::new(None, vec![]);
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let content = buffer_content(terminal.backend());
        assert!(content.contains("No adapter"));
    }

    #[test]
    fn test_adapter_found_updates_state() {
        let mut app = App::new(None, vec![]);
        assert!(app.adapter.is_none());

        // Simulate adapter being found
        app.adapter = Some(AdapterInfo {
            name: "hci0".to_string(),
            address: "AA:BB:CC:DD:EE:FF".to_string(),
            powered: true,
        });
        app.status_message = Some("Adapter found".to_string());

        assert!(app.adapter.is_some());
        assert!(app.adapter.as_ref().unwrap().powered);
        assert_eq!(app.status_message, Some("Adapter found".to_string()));

        // Now keys should work normally
        let action = app.handle_key(make_key(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(action, Action::ToggleScan);
    }
}
