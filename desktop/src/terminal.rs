use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use boomux::client::{self, Client};
use boomux::protocol::{
    AgentAttentionReason, AgentState, AttachFrame, ShellSnapshot, ShellSpec, ShellStatus,
    TerminalProfile,
};
use compact_str::CompactString;
use gpui::{Keystroke, Modifiers};
use libghostty_vt::key::{
    Action as KeyAction, Encoder as KeyEncoder, Event as KeyEvent, Key as GhosttyKey,
    Mods as GhosttyMods,
};
use libghostty_vt::kitty::graphics::{ImageFormat, PlacementIterator};
use libghostty_vt::mouse::{
    Action as MouseAction, Button as MouseButton, Encoder as MouseEncoder, EncoderSize,
    Event as MouseEvent, Position as MousePosition,
};
use libghostty_vt::render::{CellIterator, RowIterator};
use libghostty_vt::screen::CellWide;
use libghostty_vt::style::{Palette, PaletteIndex, RgbColor, Underline};
use libghostty_vt::terminal::{Mode, ScrollViewport};
use libghostty_vt::{RenderState, Terminal as GhosttyTerminal, TerminalOptions};

use crate::generated_names;
use crate::theme::TerminalTheme;

// In the pinned libghostty-vt implementation this is a page-memory budget in
// bytes, despite the Rust/C API documentation describing it as a line count.
// Ghostty allocates lazily and may exceed it to accommodate the visible screen.
const SCROLLBACK_BYTES: usize = 4 * 1024 * 1024;
const RECONNECT_ATTEMPTS: usize = 80;
const RECONNECT_DELAY: Duration = Duration::from_millis(25);
const RESIZE_SETTLE: Duration = Duration::from_millis(100);
const KITTY_IMAGE_STORAGE_BYTES: u64 = 64 * 1024 * 1024;
const EMULATOR_QUEUE_CAPACITY: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellChoice {
    pub id: String,
    pub name: String,
    pub workspace_id: String,
    pub cwd: PathBuf,
    pub status: ShellStatus,
    pub run_id: Option<String>,
}

impl ShellChoice {
    pub fn status_label(&self) -> &'static str {
        match self.status {
            ShellStatus::Pending => "pending",
            ShellStatus::Running => "running",
            ShellStatus::Exited { .. } => "exited",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceChoice {
    pub id: String,
    pub name: String,
    pub shells: Vec<ShellChoice>,
    pub agent_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentChoice {
    pub id: String,
    pub shell_name: String,
    pub workspace: String,
    pub shell_id: String,
    pub integration: String,
    pub state: AgentState,
    pub updated_at_ms: u64,
    pub needs_attention: bool,
    pub completed_attention: bool,
    pub attention_revision: Option<u64>,
}

impl AgentChoice {
    pub fn state_label(&self) -> &'static str {
        match self.state {
            AgentState::Unknown => "unknown",
            AgentState::Working => "working",
            AgentState::Blocked => "blocked",
            AgentState::Idle => "idle",
            AgentState::Inactive => "inactive",
            AgentState::Done => "finished",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BoomuxOverview {
    pub workspaces: Vec<WorkspaceChoice>,
    pub agents: Vec<AgentChoice>,
    pub focused_shell_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalCell {
    pub text: CompactString,
    pub foreground: u32,
    pub background: u32,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub wide: bool,
    pub continuation: bool,
    pub cursor: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalScreen {
    pub rows: u16,
    pub cols: u16,
    pub cells: Vec<TerminalCell>,
    pub scroll_total: u64,
    pub scroll_offset: u64,
    pub scroll_len: u64,
    pub images: Vec<TerminalImage>,
    pub image_placements: Vec<TerminalImagePlacement>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalImage {
    pub generation: u64,
    pub width: u32,
    pub height: u32,
    /// GPUI's image atlas consumes BGRA8 pixels.
    pub bgra: Arc<[u8]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalImagePlacement {
    pub image_generation: u64,
    pub viewport_col: i32,
    pub viewport_row: i32,
    pub x_offset: u32,
    pub y_offset: u32,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub source_x: u32,
    pub source_y: u32,
    pub source_width: u32,
    pub source_height: u32,
    pub cell_width: u32,
    pub cell_height: u32,
    pub z: i32,
}

struct SharedTerminal {
    screen: Mutex<Arc<TerminalScreen>>,
    updates: async_channel::Sender<()>,
    update_events: async_channel::Receiver<()>,
    emulator: Mutex<Option<mpsc::SyncSender<EmulatorCommand>>>,
    writer: Mutex<Option<std::os::unix::net::UnixStream>>,
    profile: Mutex<TerminalProfile>,
    status: Mutex<String>,
    revision: AtomicU64,
    bracketed_paste: AtomicBool,
    mouse_tracking: AtomicBool,
    pending_scroll_row: AtomicU64,
    pending_scroll_wakeup: AtomicBool,
    pending_theme: Mutex<Option<TerminalTheme>>,
    closed: AtomicBool,
}

impl SharedTerminal {
    fn new(profile: TerminalProfile) -> Self {
        let theme = crate::theme::current_terminal();
        let (updates, update_events) = async_channel::bounded(1);
        Self {
            screen: Mutex::new(Arc::new(blank_screen(profile.rows, profile.cols))),
            updates,
            update_events,
            emulator: Mutex::new(None),
            writer: Mutex::new(None),
            profile: Mutex::new(profile),
            status: Mutex::new("connecting".into()),
            revision: AtomicU64::new(1),
            bracketed_paste: AtomicBool::new(false),
            mouse_tracking: AtomicBool::new(false),
            pending_scroll_row: AtomicU64::new(0),
            pending_scroll_wakeup: AtomicBool::new(false),
            pending_theme: Mutex::new(Some(theme)),
            closed: AtomicBool::new(false),
        }
    }

    fn install_emulator(&self, sender: mpsc::SyncSender<EmulatorCommand>) {
        *self.emulator.lock().unwrap() = Some(sender);
    }

    fn emulator_command(&self, command: EmulatorCommand) -> Result<(), String> {
        self.emulator
            .lock()
            .unwrap()
            .as_ref()
            .ok_or_else(|| "Ghostty terminal core is not running".to_string())?
            .send(command)
            .map_err(|_| "Ghostty terminal core stopped".to_string())
    }

    fn try_emulator_command(&self, command: EmulatorCommand) -> Result<(), String> {
        let emulator = self.emulator.lock().unwrap();
        let sender = emulator
            .as_ref()
            .ok_or_else(|| "Ghostty terminal core is not running".to_string())?;
        match sender.try_send(command) {
            Ok(()) | Err(mpsc::TrySendError::Full(_)) => Ok(()),
            Err(mpsc::TrySendError::Disconnected(_)) => Err("Ghostty terminal core stopped".into()),
        }
    }

    fn try_key_command(&self, keystroke: Keystroke, action: KeyAction) -> Result<(), String> {
        let emulator = self.emulator.lock().unwrap();
        let sender = emulator
            .as_ref()
            .ok_or_else(|| "Ghostty terminal core is not running".to_string())?;
        match sender.try_send(EmulatorCommand::Key { keystroke, action }) {
            Ok(()) => Ok(()),
            Err(mpsc::TrySendError::Full(_)) => Err("terminal input queue is full".into()),
            Err(mpsc::TrySendError::Disconnected(_)) => Err("Ghostty terminal core stopped".into()),
        }
    }

    fn set_theme(&self, theme: TerminalTheme) -> Result<(), String> {
        *self.pending_theme.lock().unwrap() = Some(theme);
        let emulator = self.emulator.lock().unwrap();
        let sender = emulator
            .as_ref()
            .ok_or_else(|| "Ghostty terminal core is not running".to_string())?;
        match sender.try_send(EmulatorCommand::ThemeLatest) {
            Ok(()) | Err(mpsc::TrySendError::Full(_)) => Ok(()),
            Err(mpsc::TrySendError::Disconnected(_)) => Err("Ghostty terminal core stopped".into()),
        }
    }

    fn scroll_to_row(&self, row: usize) -> Result<(), String> {
        self.pending_scroll_row.store(row as u64, Ordering::Release);
        if self.pending_scroll_wakeup.swap(true, Ordering::AcqRel) {
            return Ok(());
        }

        let result = {
            let emulator = self.emulator.lock().unwrap();
            let Some(sender) = emulator.as_ref() else {
                self.pending_scroll_wakeup.store(false, Ordering::Release);
                return Err("Ghostty terminal core is not running".to_string());
            };
            sender.try_send(EmulatorCommand::ScrollLatest)
        };
        match result {
            Ok(()) | Err(mpsc::TrySendError::Full(_)) => Ok(()),
            Err(mpsc::TrySendError::Disconnected(_)) => {
                self.pending_scroll_wakeup.store(false, Ordering::Release);
                Err("Ghostty terminal core stopped".to_string())
            }
        }
    }

    fn process(&self, bytes: Vec<u8>) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        if let Err(error) = self.emulator_command(EmulatorCommand::Output(bytes)) {
            self.close(error);
        }
    }

    #[cfg(test)]
    fn viewport_is_at_bottom(&self) -> bool {
        let screen = self.screen.lock().unwrap();
        screen.scroll_offset >= screen.scroll_total.saturating_sub(screen.scroll_len)
    }

    fn resize_emulator(&self, rows: u16, cols: u16, pixel_width: u16, pixel_height: u16) {
        let cell_width = u32::from(pixel_width / cols.max(1)).max(1);
        let cell_height = u32::from(pixel_height / rows.max(1)).max(1);
        if let Err(error) = self.emulator_command(EmulatorCommand::Resize {
            rows,
            cols,
            cell_width,
            cell_height,
        }) {
            self.close(error);
        }
    }

    fn set_status(&self, status: impl Into<String>) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        self.replace_status(status);
    }

    fn replace_status(&self, status: impl Into<String>) {
        *self.status.lock().unwrap() = status.into();
        self.bump_revision();
    }

    fn bump_revision(&self) {
        self.revision.fetch_add(1, Ordering::Release);
        // One pending event is enough: consumers always load the latest
        // immutable screen. This bounds wakeups when terminal output arrives
        // faster than GPUI can paint it.
        let _ = self.updates.try_send(());
    }

    fn install_writer(&self, stream: &std::os::unix::net::UnixStream) -> Result<(), String> {
        let writer = stream
            .try_clone()
            .map_err(|error| format!("could not clone Boomux attachment: {error}"))?;
        *self.writer.lock().unwrap() = Some(writer);
        Ok(())
    }

    fn send(&self, frame: AttachFrame) -> Result<(), String> {
        let mut writer = self.writer.lock().unwrap();
        let stream = writer
            .as_mut()
            .ok_or_else(|| "Boomux terminal is not attached".to_string())?;
        frame
            .write_to(stream)
            .map_err(|error| format!("Boomux terminal write failed: {error}"))
    }

    fn close(&self, status: impl Into<String>) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        *self.writer.lock().unwrap() = None;
        if let Some(emulator) = self.emulator.lock().unwrap().take() {
            let _ = emulator.send(EmulatorCommand::Stop);
        }
        self.replace_status(status);
    }
}

enum EmulatorCommand {
    Output(Vec<u8>),
    Key {
        keystroke: Keystroke,
        action: KeyAction,
    },
    Resize {
        rows: u16,
        cols: u16,
        cell_width: u32,
        cell_height: u32,
    },
    Scroll(ScrollViewport),
    ScrollLatest,
    ThemeLatest,
    MouseWheel {
        lines: isize,
        x: f32,
        y: f32,
        screen_width: u32,
        screen_height: u32,
        modifiers: Modifiers,
    },
    Stop,
}

pub struct TerminalSession {
    pub shell_id: String,
    pub shell_name: String,
    shared: Arc<SharedTerminal>,
    last_size: Mutex<(u16, u16)>,
}

impl TerminalSession {
    pub fn attach(
        shell: ShellChoice,
        rows: u16,
        cols: u16,
        pixel_width: u16,
        pixel_height: u16,
    ) -> Result<Self, String> {
        let client = client::connect_if_running()
            .map_err(|error| format!("could not connect to Boomux: {error}"))?
            .ok_or_else(|| "Boomux is not running".to_string())?;
        let profile = terminal_profile(rows, cols, pixel_width, pixel_height);
        let attachment = attach_shell(&client, &shell, profile.clone(), true)?;
        let shared = Arc::new(SharedTerminal::new(profile));
        let stream = attachment.stream;
        shared.install_writer(&stream)?;
        start_emulator(&shared, rows, cols, pixel_width, pixel_height)?;
        shared.process(attachment.reconstruction);

        let expected_run_id = client
            .get_shell(&shell.id)
            .ok()
            .and_then(|snapshot| snapshot.run.map(|run| run.id))
            .or(shell.run_id.clone());
        spawn_reader(
            client,
            shell.id.clone(),
            expected_run_id,
            stream,
            Arc::clone(&shared),
        );
        // Attachment warnings describe daemon-side environment history; they
        // are diagnostic metadata, not actionable terminal state. Keep pane
        // headings quiet after a successful attachment while still surfacing
        // later connection and emulator failures through `status_message`.
        shared.set_status("attached");

        Ok(Self {
            shell_id: shell.id,
            shell_name: shell.name,
            shared,
            // Force one normal resize from the first GPUI render pass. The
            // attachment profile already established this size; repeating it
            // here avoids blocking startup on the old two-step resize nudge.
            last_size: Mutex::new((0, 0)),
        })
    }

    pub fn revision(&self) -> u64 {
        self.shared.revision.load(Ordering::Acquire)
    }

    pub fn set_theme(&self, theme: TerminalTheme) -> Result<(), String> {
        self.shared.set_theme(theme)
    }

    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Acquire)
    }

    pub fn status_message(&self) -> Option<String> {
        let status = self.shared.status.lock().unwrap();
        match status.as_str() {
            "attached" | "connecting" => None,
            status => Some(
                status
                    .strip_prefix("attached · ")
                    .unwrap_or(status)
                    .to_string(),
            ),
        }
    }

    pub fn screen(&self) -> Arc<TerminalScreen> {
        Arc::clone(&self.shared.screen.lock().unwrap())
    }

    pub fn update_events(&self) -> async_channel::Receiver<()> {
        self.shared.update_events.clone()
    }

    pub fn send_key(&self, keystroke: &Keystroke, action: KeyAction) -> bool {
        if !terminal_key_supported(keystroke) {
            return false;
        }
        if let Err(error) = self.shared.try_key_command(keystroke.clone(), action) {
            self.shared.set_status(error);
        }
        true
    }

    pub fn paste(&self, text: &str) -> bool {
        if text.is_empty() {
            return false;
        }
        let bytes = encode_paste(text, self.shared.bracketed_paste.load(Ordering::Acquire));
        if let Err(error) = self.shared.send(AttachFrame::Input(bytes)) {
            self.shared.set_status(error);
        }
        true
    }

    pub fn scroll(&self, lines: isize) -> bool {
        if lines == 0 {
            return false;
        }
        if let Err(error) = self
            .shared
            .emulator_command(EmulatorCommand::Scroll(ScrollViewport::Delta(lines)))
        {
            self.shared.set_status(error);
        }
        true
    }

    pub fn report_mouse_wheel(
        &self,
        lines: isize,
        position: (f32, f32),
        screen_size: (u32, u32),
        modifiers: Modifiers,
    ) -> bool {
        if lines == 0 || !self.shared.mouse_tracking.load(Ordering::Acquire) {
            return false;
        }
        if let Err(error) = self
            .shared
            .try_emulator_command(EmulatorCommand::MouseWheel {
                lines: lines.clamp(-32, 32),
                x: position.0,
                y: position.1,
                screen_width: screen_size.0,
                screen_height: screen_size.1,
                modifiers,
            })
        {
            self.shared.set_status(error);
        }
        true
    }

    pub fn scroll_to(&self, row: usize) {
        if let Err(error) = self.shared.scroll_to_row(row) {
            self.shared.set_status(error);
        }
    }

    pub fn scroll_to_top(&self) {
        if let Err(error) = self
            .shared
            .emulator_command(EmulatorCommand::Scroll(ScrollViewport::Top))
        {
            self.shared.set_status(error);
        }
    }

    pub fn scroll_to_bottom(&self) {
        if let Err(error) = self
            .shared
            .emulator_command(EmulatorCommand::Scroll(ScrollViewport::Bottom))
        {
            self.shared.set_status(error);
        }
    }

    pub fn resize(&self, rows: u16, cols: u16, pixel_width: u16, pixel_height: u16) -> bool {
        let mut last_size = self.last_size.lock().unwrap();
        if *last_size == (rows, cols) {
            return false;
        }
        *last_size = (rows, cols);
        {
            let mut profile = self.shared.profile.lock().unwrap();
            profile.rows = rows;
            profile.cols = cols;
            profile.pixel_width = pixel_width;
            profile.pixel_height = pixel_height;
        }
        self.shared
            .resize_emulator(rows, cols, pixel_width, pixel_height);
        if let Err(error) = self.shared.send(AttachFrame::Resize {
            rows,
            cols,
            pixel_width,
            pixel_height,
        }) {
            self.shared.set_status(error);
        } else {
            self.shared.bump_revision();
        }
        true
    }

    pub fn focus(&self) {
        if let Err(error) = self.shared.send(AttachFrame::FocusGained) {
            self.shared.set_status(error);
        }
    }
}

fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    if !bracketed {
        return normalized.into_bytes();
    }
    let mut bytes = Vec::with_capacity(normalized.len() + 12);
    bytes.extend_from_slice(b"\x1b[200~");
    bytes.extend_from_slice(normalized.replace("\x1b[201~", "").as_bytes());
    bytes.extend_from_slice(b"\x1b[201~");
    bytes
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.shared.send(AttachFrame::Detached);
        self.shared.closed.store(true, Ordering::Release);
        if let Some(emulator) = self.shared.emulator.lock().unwrap().take() {
            let _ = emulator.send(EmulatorCommand::Stop);
        }
    }
}

pub fn discover_overview() -> Result<BoomuxOverview, String> {
    let Some(client) = client::connect_if_running()
        .map_err(|error| format!("could not connect to Boomux: {error}"))?
    else {
        return Err("Boomux is not running".into());
    };
    let snapshot = client
        .snapshot()
        .map_err(|error| format!("could not read Boomux workspaces: {error}"))?;
    let focused_shell_id = snapshot
        .focused_terminal
        .as_ref()
        .map(|focused| focused.shell_id.clone());
    let mut workspaces = Vec::new();
    let mut agents = Vec::new();
    for workspace in &snapshot.workspaces {
        let shells = workspace
            .shells
            .iter()
            .cloned()
            .map(shell_choice)
            .collect::<Vec<_>>();
        let visible_agents = workspace.agents.iter().filter(|agent| {
            let attached_to_current_run = workspace.shells.iter().any(|shell| {
                shell.id == agent.shell_id
                    && shell.run.as_ref().is_some_and(|run| run.id == agent.run_id)
            });
            agent_is_visible(
                agent.observation.state,
                agent.attention.is_some(),
                attached_to_current_run,
            )
        });
        let agent_count = visible_agents.clone().count();
        agents.extend(visible_agents.map(|agent| {
            let attention_revision = agent
                .attention
                .as_ref()
                .map(|attention| attention.observation.revision);
            let completed_attention = agent
                .attention
                .as_ref()
                .is_some_and(|attention| attention.reason == AgentAttentionReason::Completed);
            let needs_attention = agent
                .attention
                .as_ref()
                .is_some_and(|attention| attention.reason == AgentAttentionReason::Blocked);
            AgentChoice {
                id: agent.id.clone(),
                shell_name: workspace
                    .shells
                    .iter()
                    .find(|shell| shell.id == agent.shell_id)
                    .map(|shell| shell.name.clone())
                    .unwrap_or_else(|| agent.name.clone()),
                workspace: workspace.name.clone(),
                shell_id: agent.shell_id.clone(),
                integration: agent.integration.clone(),
                state: agent.observation.state,
                updated_at_ms: agent.attention.as_ref().map_or(
                    agent.observation.observed_at_ms,
                    |attention| {
                        agent
                            .observation
                            .observed_at_ms
                            .max(attention.observation.observed_at_ms)
                    },
                ),
                needs_attention,
                completed_attention,
                attention_revision,
            }
        }));
        workspaces.push(WorkspaceChoice {
            id: workspace.id.clone(),
            name: workspace.name.clone(),
            shells,
            agent_count,
        });
    }
    agents.sort_by_key(|agent| std::cmp::Reverse(agent.updated_at_ms));
    Ok(BoomuxOverview {
        workspaces,
        agents,
        focused_shell_id,
    })
}

pub fn acknowledge_agent_attention(
    agent_id: &str,
    observation_revision: u64,
) -> Result<(), String> {
    let Some(client) = client::connect_if_running()
        .map_err(|error| format!("could not connect to Boomux: {error}"))?
    else {
        return Err("Boomux is not running".into());
    };
    client
        .acknowledge_agent_attention(agent_id, observation_revision)
        .map(|_| ())
        .map_err(|error| format!("could not acknowledge Agent notification: {error}"))
}

fn agent_is_visible(state: AgentState, has_attention: bool, attached_to_current_run: bool) -> bool {
    has_attention
        || attached_to_current_run && !matches!(state, AgentState::Inactive | AgentState::Done)
}

fn shell_choice(shell: ShellSnapshot) -> ShellChoice {
    ShellChoice {
        id: shell.id,
        name: shell.name,
        workspace_id: shell.workspace_id,
        cwd: shell.cwd,
        status: shell.status,
        run_id: shell.run.map(|run| run.id),
    }
}

/// Create a pending shell next to an existing shell. Boomux remains the owner
/// of the PTY; the caller can immediately attach the returned choice.
pub fn create_shell(anchor: &ShellChoice) -> Result<ShellChoice, String> {
    let Some(client) = client::connect_if_running()
        .map_err(|error| format!("could not connect to Boomux: {error}"))?
    else {
        return Err("Boomux is not running".into());
    };
    let workspace = client
        .get_workspace(&anchor.workspace_id)
        .map_err(|error| format!("could not read Boomux workspace: {error}"))?;
    let name =
        generated_names::random_excluding(workspace.shells.iter().map(|shell| shell.name.as_str()))
            .ok_or_else(|| "Boomux shell names are exhausted".to_string())?;
    let shell = client
        .create_shell(
            &workspace.id,
            ShellSpec::login(
                name,
                workspace.default_cwd.unwrap_or_else(|| anchor.cwd.clone()),
            ),
        )
        .map_err(|error| format!("could not create Boomux shell: {error}"))?;
    Ok(shell_choice(shell))
}

pub fn create_shell_in_workspace(workspace_id: &str) -> Result<ShellChoice, String> {
    let Some(client) = client::connect_if_running()
        .map_err(|error| format!("could not connect to Boomux: {error}"))?
    else {
        return Err("Boomux is not running".into());
    };
    let workspace = client
        .get_workspace(workspace_id)
        .map_err(|error| format!("could not read Boomux workspace: {error}"))?;
    let name =
        generated_names::random_excluding(workspace.shells.iter().map(|shell| shell.name.as_str()))
            .ok_or_else(|| "Boomux shell names are exhausted".to_string())?;
    let cwd = workspace
        .default_cwd
        .or_else(|| workspace.shells.first().map(|shell| shell.cwd.clone()))
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| "could not determine a working directory for the new shell".to_string())?;
    client
        .create_shell(&workspace.id, ShellSpec::login(name, cwd))
        .map(shell_choice)
        .map_err(|error| format!("could not create Boomux shell: {error}"))
}

/// Create a local Workspace with its first pending login Shell. Boomux owns
/// both resources; the returned Shell can be attached immediately.
pub fn create_workspace_with_shell() -> Result<ShellChoice, String> {
    let Some(client) = client::connect_if_running()
        .map_err(|error| format!("could not connect to Boomux: {error}"))?
    else {
        return Err("Boomux is not running".into());
    };
    let snapshot = client
        .snapshot()
        .map_err(|error| format!("could not read Boomux workspaces: {error}"))?;
    let workspace_name = generated_names::random_excluding(
        snapshot
            .workspaces
            .iter()
            .map(|workspace| workspace.name.as_str()),
    )
    .ok_or_else(|| "Boomux workspace names are exhausted".to_string())?;
    let shell_name = generated_names::random_excluding(std::iter::empty())
        .ok_or_else(|| "Boomux shell names are exhausted".to_string())?;
    let cwd = std::env::current_dir()
        .map_err(|error| format!("could not determine the new workspace directory: {error}"))?;
    let workspace = client
        .create_workspace_with_default_cwd(
            workspace_name,
            Some(cwd.clone()),
            vec![ShellSpec::login(shell_name, cwd)],
        )
        .map_err(|error| format!("could not create Boomux workspace: {error}"))?;
    workspace
        .shells
        .into_iter()
        .next()
        .map(shell_choice)
        .ok_or_else(|| "Boomux created the workspace without its initial shell".to_string())
}

pub fn rename_workspace(workspace_id: &str, name: &str) -> Result<(), String> {
    let Some(client) = client::connect_if_running()
        .map_err(|error| format!("could not connect to Boomux: {error}"))?
    else {
        return Err("Boomux is not running".into());
    };
    client
        .rename_workspace(workspace_id, name)
        .map_err(|error| format!("could not rename Boomux workspace: {error}"))
}

pub fn rename_shell(shell_id: &str, name: &str) -> Result<(), String> {
    let Some(client) = client::connect_if_running()
        .map_err(|error| format!("could not connect to Boomux: {error}"))?
    else {
        return Err("Boomux is not running".into());
    };
    client
        .rename_shell(shell_id, name)
        .map_err(|error| format!("could not rename Boomux shell: {error}"))
}

pub fn remove_workspace(workspace_id: &str) -> Result<(), String> {
    let Some(client) = client::connect_if_running()
        .map_err(|error| format!("could not connect to Boomux: {error}"))?
    else {
        return Err("Boomux is not running".into());
    };
    client
        .close_workspace(workspace_id)
        .map_err(|error| format!("could not remove Boomux workspace: {error}"))
}

pub fn close_shell(shell_id: &str) -> Result<(), String> {
    let Some(client) = client::connect_if_running()
        .map_err(|error| format!("could not connect to Boomux: {error}"))?
    else {
        return Err("Boomux is not running".into());
    };
    client
        .close_shell(shell_id)
        .map_err(|error| format!("could not close Boomux shell: {error}"))
}

fn terminal_profile(rows: u16, cols: u16, pixel_width: u16, pixel_height: u16) -> TerminalProfile {
    TerminalProfile {
        term: Some("xterm-256color".into()),
        colorterm: Some("truecolor".into()),
        term_program: Some("boomux-desktop".into()),
        term_program_version: Some(env!("CARGO_PKG_VERSION").into()),
        rows,
        cols,
        pixel_width,
        pixel_height,
    }
}

fn attach_shell(
    client: &Client,
    shell: &ShellChoice,
    profile: TerminalProfile,
    takeover: bool,
) -> Result<client::Attachment, String> {
    let result = match (&shell.status, shell.run_id.as_deref()) {
        (ShellStatus::Running, Some(run_id)) => {
            client.attach_exact_run_with_client_environment(&shell.id, run_id, takeover, profile)
        }
        (ShellStatus::Pending, _) => {
            client.attach_with_client_environment(&shell.id, takeover, profile)
        }
        (ShellStatus::Exited { .. }, _) => {
            client.attach_restarting_with_client_environment(&shell.id, takeover, profile)
        }
        (ShellStatus::Running, None) => client.attach(&shell.id, takeover, profile),
    };
    result.map_err(|error| format!("could not attach {}: {error}", shell.name))
}

struct EmulatorCore {
    terminal: GhosttyTerminal<'static, 'static>,
    render_state: RenderState<'static>,
    rows: RowIterator<'static>,
    cells: CellIterator<'static>,
    graphics: PlacementIterator<'static>,
    key: KeyEncoder<'static>,
    mouse: MouseEncoder<'static>,
    previous_images: Vec<TerminalImage>,
    cell_width: u32,
    cell_height: u32,
}

impl EmulatorCore {
    fn new(
        shared: &Arc<SharedTerminal>,
        rows: u16,
        cols: u16,
        pixel_width: u16,
        pixel_height: u16,
    ) -> Result<Self, String> {
        let mut terminal = GhosttyTerminal::new(TerminalOptions {
            // Start at a sentinel size so the first resize also records pixel
            // dimensions; libghostty treats an unchanged cell size as a no-op.
            cols: 1,
            rows: 1,
            max_scrollback: SCROLLBACK_BYTES,
        })
        .map_err(|error| format!("could not create Ghostty terminal: {error}"))?;
        let theme = shared
            .pending_theme
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| "terminal theme was not initialized".to_string())?;
        configure_terminal(&mut terminal, theme)?;
        terminal
            .set_kitty_image_storage_limit(KITTY_IMAGE_STORAGE_BYTES)
            .map_err(|error| format!("could not enable Ghostty Kitty graphics: {error}"))?;

        let weak = Arc::downgrade(shared);
        terminal
            .on_pty_write(move |_, bytes| {
                if let Some(shared) = weak.upgrade()
                    && let Err(error) = shared.send(AttachFrame::Input(bytes.to_vec()))
                {
                    shared.set_status(error);
                }
            })
            .map_err(|error| format!("could not configure Ghostty PTY replies: {error}"))?;
        let cell_width = cell_dimension(pixel_width, cols);
        let cell_height = cell_dimension(pixel_height, rows);
        terminal
            .resize(cols, rows, cell_width, cell_height)
            .map_err(|error| format!("could not size Ghostty terminal: {error}"))?;

        Ok(Self {
            terminal,
            render_state: RenderState::new()
                .map_err(|error| format!("could not create Ghostty render state: {error}"))?,
            rows: RowIterator::new()
                .map_err(|error| format!("could not create Ghostty row iterator: {error}"))?,
            cells: CellIterator::new()
                .map_err(|error| format!("could not create Ghostty cell iterator: {error}"))?,
            graphics: PlacementIterator::new()
                .map_err(|error| format!("could not create Ghostty graphics iterator: {error}"))?,
            key: KeyEncoder::new()
                .map_err(|error| format!("could not create Ghostty key encoder: {error}"))?,
            mouse: MouseEncoder::new()
                .map_err(|error| format!("could not create Ghostty mouse encoder: {error}"))?,
            previous_images: Vec::new(),
            cell_width,
            cell_height,
        })
    }

    fn apply(&mut self, command: EmulatorCommand) -> Result<bool, String> {
        match command {
            EmulatorCommand::Output(bytes) => self.terminal.vt_write(&bytes),
            EmulatorCommand::Key { .. } => {
                unreachable!("key events are resolved by the emulator worker")
            }
            EmulatorCommand::Resize {
                rows,
                cols,
                cell_width,
                cell_height,
            } => {
                self.terminal
                    .resize(cols, rows, cell_width, cell_height)
                    .map_err(|error| format!("Ghostty resize failed: {error}"))?;
                self.cell_width = cell_width;
                self.cell_height = cell_height;
            }
            EmulatorCommand::Scroll(viewport) => self.terminal.scroll_viewport(viewport),
            EmulatorCommand::ScrollLatest => {
                unreachable!("latest scroll requests are resolved by the emulator worker")
            }
            EmulatorCommand::ThemeLatest => {
                unreachable!("latest theme requests are resolved by the emulator worker")
            }
            EmulatorCommand::MouseWheel { .. } => {
                unreachable!("mouse events are resolved by the emulator worker")
            }
            EmulatorCommand::Stop => return Ok(false),
        }
        Ok(true)
    }

    fn screen(&mut self) -> Result<TerminalScreen, String> {
        let (images, image_placements) = terminal_images(
            &self.terminal,
            &mut self.graphics,
            self.cell_width,
            self.cell_height,
            &self.previous_images,
        )?;
        self.previous_images = images.clone();
        let scrollbar = self
            .terminal
            .scrollbar()
            .map_err(|error| format!("could not read Ghostty scrollbar: {error}"))?;
        let snapshot = self
            .render_state
            .update(&self.terminal)
            .map_err(|error| format!("Ghostty render update failed: {error}"))?;
        let rows = snapshot
            .rows()
            .map_err(|error| format!("could not read Ghostty rows: {error}"))?;
        let cols = snapshot
            .cols()
            .map_err(|error| format!("could not read Ghostty columns: {error}"))?;
        let colors = snapshot
            .colors()
            .map_err(|error| format!("could not read Ghostty colors: {error}"))?;
        let cursor = if snapshot
            .cursor_visible()
            .map_err(|error| format!("could not read Ghostty cursor visibility: {error}"))?
        {
            snapshot
                .cursor_viewport()
                .map_err(|error| format!("could not read Ghostty cursor: {error}"))?
        } else {
            None
        };

        let mut output = Vec::with_capacity(usize::from(rows) * usize::from(cols));
        let mut cell_text = String::new();
        let mut row_iter = self
            .rows
            .update(&snapshot)
            .map_err(|error| format!("could not iterate Ghostty rows: {error}"))?;
        let mut y = 0_u16;
        while let Some(row) = row_iter.next() {
            let mut cell_iter = self
                .cells
                .update(row)
                .map_err(|error| format!("could not iterate Ghostty cells: {error}"))?;
            let mut x = 0_u16;
            while let Some(cell) = cell_iter.next() {
                let style = cell
                    .style()
                    .map_err(|error| format!("could not read Ghostty cell style: {error}"))?;
                cell_text.clear();
                cell.graphemes_utf8(&mut cell_text)
                    .map_err(|error| format!("could not read Ghostty cell text: {error}"))?;
                if cell_text.is_empty() || style.invisible {
                    cell_text.push(' ');
                }
                let wide = cell
                    .raw_cell()
                    .and_then(|cell| cell.wide())
                    .map_err(|error| format!("could not read Ghostty cell width: {error}"))?;
                let mut foreground = cell
                    .fg_color()
                    .map_err(|error| format!("could not read Ghostty foreground: {error}"))?
                    .unwrap_or(colors.foreground);
                let mut background = cell
                    .bg_color()
                    .map_err(|error| format!("could not read Ghostty background: {error}"))?
                    .unwrap_or(colors.background);
                if style.inverse {
                    std::mem::swap(&mut foreground, &mut background);
                }
                output.push(TerminalCell {
                    text: CompactString::from(cell_text.as_str()),
                    foreground: rgb_value(foreground),
                    background: rgb_value(background),
                    bold: style.bold,
                    italic: style.italic,
                    underline: style.underline != Underline::None,
                    wide: wide == CellWide::Wide,
                    continuation: matches!(wide, CellWide::SpacerTail | CellWide::SpacerHead),
                    cursor: cursor.is_some_and(|cursor| cursor.x == x && cursor.y == y),
                });
                x = x.saturating_add(1);
            }
            y = y.saturating_add(1);
        }

        Ok(TerminalScreen {
            rows,
            cols,
            cells: output,
            scroll_total: scrollbar.total,
            scroll_offset: scrollbar.offset,
            scroll_len: scrollbar.len,
            images,
            image_placements,
        })
    }
}

fn apply_emulator_command(
    core: &mut EmulatorCore,
    shared: &SharedTerminal,
    command: EmulatorCommand,
) -> Result<bool, String> {
    match command {
        EmulatorCommand::Key { keystroke, action } => {
            // Typing follows conventional terminal behavior and returns the
            // viewport to the live prompt before the PTY produces more output.
            if action != KeyAction::Release {
                core.terminal.scroll_viewport(ScrollViewport::Bottom);
            }
            let bytes = encode_key(&core.terminal, &mut core.key, &keystroke, action)?;
            if !bytes.is_empty() {
                shared.send(AttachFrame::Input(bytes))?;
            }
            Ok(true)
        }
        EmulatorCommand::ScrollLatest => {
            shared.pending_scroll_wakeup.store(false, Ordering::Release);
            let row = shared.pending_scroll_row.load(Ordering::Acquire) as usize;
            core.apply(EmulatorCommand::Scroll(ScrollViewport::Row(row)))
        }
        EmulatorCommand::ThemeLatest => {
            if let Some(theme) = shared.pending_theme.lock().unwrap().take() {
                configure_terminal(&mut core.terminal, theme)?;
            }
            Ok(true)
        }
        EmulatorCommand::MouseWheel {
            lines,
            x,
            y,
            screen_width,
            screen_height,
            modifiers,
        } => {
            let bytes = encode_mouse_wheel(
                &core.terminal,
                &mut core.mouse,
                lines,
                (x, y),
                (screen_width, screen_height),
                core.cell_width,
                core.cell_height,
                modifiers,
            )?;
            if !bytes.is_empty() {
                shared.send(AttachFrame::Input(bytes))?;
            }
            Ok(true)
        }
        command => core.apply(command),
    }
}

#[allow(clippy::too_many_arguments)]
fn encode_mouse_wheel(
    terminal: &GhosttyTerminal<'_, '_>,
    encoder: &mut MouseEncoder<'_>,
    lines: isize,
    position: (f32, f32),
    screen_size: (u32, u32),
    cell_width: u32,
    cell_height: u32,
    modifiers: Modifiers,
) -> Result<Vec<u8>, String> {
    if lines == 0
        || !terminal
            .is_mouse_tracking()
            .map_err(|error| format!("could not read Ghostty mouse mode: {error}"))?
    {
        return Ok(Vec::new());
    }
    let mut mods = GhosttyMods::empty();
    mods.set(GhosttyMods::SHIFT, modifiers.shift);
    mods.set(GhosttyMods::ALT, modifiers.alt);
    mods.set(GhosttyMods::CTRL, modifiers.control);
    mods.set(GhosttyMods::SUPER, modifiers.platform);
    let (screen_width, screen_height) = screen_size;
    encoder
        .set_options_from_terminal(terminal)
        .set_size(EncoderSize {
            screen_width,
            screen_height,
            cell_width,
            cell_height,
            padding_top: 8,
            padding_bottom: 8,
            padding_right: 8,
            padding_left: 8,
        });
    let mut event = MouseEvent::new()
        .map_err(|error| format!("could not create Ghostty mouse event: {error}"))?;
    event
        .set_action(MouseAction::Press)
        .set_button(Some(if lines > 0 {
            MouseButton::Four
        } else {
            MouseButton::Five
        }))
        .set_mods(mods)
        .set_position(MousePosition {
            x: position.0,
            y: position.1,
        });
    let mut bytes = Vec::with_capacity(lines.unsigned_abs().min(32) * 16);
    for _ in 0..lines.unsigned_abs().min(32) {
        encoder
            .encode_to_vec(&event, &mut bytes)
            .map_err(|error| format!("could not encode Ghostty mouse wheel: {error}"))?;
    }
    Ok(bytes)
}

fn terminal_images(
    terminal: &GhosttyTerminal<'_, '_>,
    iterator: &mut PlacementIterator<'_>,
    cell_width: u32,
    cell_height: u32,
    previous_images: &[TerminalImage],
) -> Result<(Vec<TerminalImage>, Vec<TerminalImagePlacement>), String> {
    let graphics = terminal
        .kitty_graphics()
        .map_err(|error| format!("could not read Ghostty graphics: {error}"))?;
    let mut iteration = iterator
        .update(&graphics)
        .map_err(|error| format!("could not iterate Ghostty graphics: {error}"))?;
    let mut images = Vec::new();
    let mut image_placements = Vec::new();
    let mut copied_generations = HashSet::new();

    while let Some(placement) = iteration.next() {
        let image_id = placement
            .image_id()
            .map_err(|error| format!("could not read Ghostty image id: {error}"))?;
        let Some(image) = graphics.image(image_id) else {
            continue;
        };
        let info = placement
            .placement_render_info(&image, terminal)
            .map_err(|error| format!("could not read Ghostty image placement: {error}"))?;
        if !info.viewport_visible {
            continue;
        }
        let generation = image
            .generation()
            .map_err(|error| format!("could not read Ghostty image generation: {error}"))?;
        if copied_generations.insert(generation) {
            if let Some(previous) = previous_images
                .iter()
                .find(|previous| previous.generation == generation)
            {
                images.push(previous.clone());
            } else {
                let width = image
                    .width()
                    .map_err(|error| format!("could not read Ghostty image width: {error}"))?;
                let height = image
                    .height()
                    .map_err(|error| format!("could not read Ghostty image height: {error}"))?;
                let format = image
                    .format()
                    .map_err(|error| format!("could not read Ghostty image format: {error}"))?;
                let data = image
                    .data()
                    .map_err(|error| format!("could not read Ghostty image pixels: {error}"))?;
                images.push(TerminalImage {
                    generation,
                    width,
                    height,
                    bgra: image_bgra(format, width, height, data)?.into(),
                });
            }
        }
        image_placements.push(TerminalImagePlacement {
            image_generation: generation,
            viewport_col: info.viewport_col,
            viewport_row: info.viewport_row,
            x_offset: placement
                .x_offset()
                .map_err(|error| format!("could not read Ghostty image x offset: {error}"))?,
            y_offset: placement
                .y_offset()
                .map_err(|error| format!("could not read Ghostty image y offset: {error}"))?,
            pixel_width: info.pixel_width,
            pixel_height: info.pixel_height,
            source_x: info.source_x,
            source_y: info.source_y,
            source_width: info.source_width,
            source_height: info.source_height,
            cell_width,
            cell_height,
            z: placement
                .z()
                .map_err(|error| format!("could not read Ghostty image z-index: {error}"))?,
        });
    }

    Ok((images, image_placements))
}

fn image_bgra(
    format: ImageFormat,
    width: u32,
    height: u32,
    data: &[u8],
) -> Result<Vec<u8>, String> {
    let pixel_count = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .ok_or_else(|| "Ghostty image dimensions are too large".to_string())?;
    let channels = match format {
        ImageFormat::Rgb => 3,
        ImageFormat::Rgba => 4,
        ImageFormat::GrayAlpha => 2,
        ImageFormat::Gray => 1,
        ImageFormat::Png => return Err("Ghostty returned undecoded PNG image data".into()),
        _ => return Err("Ghostty returned an unsupported image format".into()),
    };
    if data.len() != pixel_count.saturating_mul(channels) {
        return Err(format!(
            "Ghostty image has {} bytes; expected {}",
            data.len(),
            pixel_count.saturating_mul(channels)
        ));
    }

    let mut bgra = Vec::with_capacity(pixel_count.saturating_mul(4));
    for pixel in data.chunks_exact(channels) {
        match format {
            ImageFormat::Rgb => bgra.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]),
            ImageFormat::Rgba => bgra.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]),
            ImageFormat::GrayAlpha => {
                bgra.extend_from_slice(&[pixel[0], pixel[0], pixel[0], pixel[1]])
            }
            ImageFormat::Gray => bgra.extend_from_slice(&[pixel[0], pixel[0], pixel[0], 255]),
            ImageFormat::Png => unreachable!(),
            _ => unreachable!(),
        }
    }
    Ok(bgra)
}

fn start_emulator(
    shared: &Arc<SharedTerminal>,
    rows: u16,
    cols: u16,
    pixel_width: u16,
    pixel_height: u16,
) -> Result<(), String> {
    let (sender, receiver) = mpsc::sync_channel(EMULATOR_QUEUE_CAPACITY);
    let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
    let worker_shared = Arc::clone(shared);
    thread::Builder::new()
        .name("boomux-ghostty-terminal".into())
        .spawn(move || {
            let mut core =
                match EmulatorCore::new(&worker_shared, rows, cols, pixel_width, pixel_height) {
                    Ok(core) => core,
                    Err(error) => {
                        let _ = ready_sender.send(Err(error));
                        return;
                    }
                };
            if let Err(error) = publish_screen(&mut core, &worker_shared) {
                let _ = ready_sender.send(Err(error));
                return;
            }
            if ready_sender.send(Ok(())).is_err() {
                return;
            }

            while let Ok(command) = receiver.recv() {
                match apply_emulator_command(&mut core, &worker_shared, command) {
                    Ok(true) => {}
                    Ok(false) => return,
                    Err(error) => {
                        worker_shared.close(error);
                        return;
                    }
                }

                let mut stopped = false;
                while let Ok(command) = receiver.try_recv() {
                    match apply_emulator_command(&mut core, &worker_shared, command) {
                        Ok(true) => {}
                        Ok(false) => {
                            stopped = true;
                            break;
                        }
                        Err(error) => {
                            worker_shared.close(error);
                            return;
                        }
                    }
                }
                if stopped {
                    return;
                }
                // A full queue could not accept the wake-up marker, but it did
                // keep the latest requested row. Apply that row once after the
                // lossless command queue has been drained.
                if worker_shared
                    .pending_scroll_wakeup
                    .swap(false, Ordering::AcqRel)
                {
                    let row = worker_shared.pending_scroll_row.load(Ordering::Acquire) as usize;
                    if let Err(error) =
                        core.apply(EmulatorCommand::Scroll(ScrollViewport::Row(row)))
                    {
                        worker_shared.close(error);
                        return;
                    }
                }
                if let Some(theme) = worker_shared.pending_theme.lock().unwrap().take()
                    && let Err(error) = configure_terminal(&mut core.terminal, theme)
                {
                    worker_shared.close(error);
                    return;
                }
                if core.terminal.mode(Mode::SYNC_OUTPUT).unwrap_or(false) {
                    continue;
                }
                if let Err(error) = publish_screen(&mut core, &worker_shared) {
                    worker_shared.close(error);
                    return;
                }
            }
        })
        .map_err(|error| format!("could not start Ghostty terminal worker: {error}"))?;

    ready_receiver
        .recv()
        .map_err(|_| "Ghostty terminal worker stopped during startup".to_string())??;
    shared.install_emulator(sender);
    Ok(())
}

fn publish_screen(core: &mut EmulatorCore, shared: &SharedTerminal) -> Result<(), String> {
    let screen = core.screen()?;
    *shared.screen.lock().unwrap() = Arc::new(screen);
    let bracketed_paste = core
        .terminal
        .mode(Mode::BRACKETED_PASTE)
        .map_err(|error| format!("could not read Ghostty bracketed paste mode: {error}"))?;
    shared
        .bracketed_paste
        .store(bracketed_paste, Ordering::Release);
    let mouse_tracking = core
        .terminal
        .is_mouse_tracking()
        .map_err(|error| format!("could not read Ghostty mouse mode: {error}"))?;
    shared
        .mouse_tracking
        .store(mouse_tracking, Ordering::Release);
    shared.bump_revision();
    Ok(())
}

fn configure_terminal(
    terminal: &mut GhosttyTerminal<'_, '_>,
    theme: TerminalTheme,
) -> Result<(), String> {
    let mut palette = Palette::default();
    for index in 0..=u8::MAX {
        palette.set(
            PaletteIndex(index),
            rgb_color(indexed_color_with_palette(index, &theme.ansi)),
        );
    }
    terminal
        .set_default_fg_color(Some(rgb_color(theme.foreground)))
        .and_then(|terminal| terminal.set_default_bg_color(Some(rgb_color(theme.background))))
        .and_then(|terminal| terminal.set_default_cursor_color(Some(rgb_color(theme.cursor))))
        .and_then(|terminal| terminal.set_default_color_palette(Some(palette)))
        .map_err(|error| format!("could not configure Ghostty colors: {error}"))?;
    Ok(())
}

fn blank_screen(rows: u16, cols: u16) -> TerminalScreen {
    let theme = crate::theme::current_terminal();
    TerminalScreen {
        rows,
        cols,
        cells: vec![
            TerminalCell {
                text: " ".into(),
                foreground: theme.foreground,
                background: theme.background,
                bold: false,
                italic: false,
                underline: false,
                wide: false,
                continuation: false,
                cursor: false,
            };
            usize::from(rows) * usize::from(cols)
        ],
        scroll_total: u64::from(rows),
        scroll_offset: 0,
        scroll_len: u64::from(rows),
        images: Vec::new(),
        image_placements: Vec::new(),
    }
}

fn cell_dimension(pixels: u16, cells: u16) -> u32 {
    u32::from(pixels / cells.max(1)).max(1)
}

fn rgb_color(value: u32) -> RgbColor {
    RgbColor {
        r: ((value >> 16) & 0xff) as u8,
        g: ((value >> 8) & 0xff) as u8,
        b: (value & 0xff) as u8,
    }
}

fn rgb_value(color: RgbColor) -> u32 {
    (u32::from(color.r) << 16) | (u32::from(color.g) << 8) | u32::from(color.b)
}

fn spawn_reader(
    client: Client,
    shell_id: String,
    expected_run_id: Option<String>,
    mut stream: std::os::unix::net::UnixStream,
    shared: Arc<SharedTerminal>,
) {
    thread::Builder::new()
        .name("boomux-desktop-terminal".into())
        .spawn(move || {
            loop {
                match AttachFrame::read_from(&mut stream) {
                    Ok(AttachFrame::Output(bytes)) => shared.process(bytes),
                    Ok(AttachFrame::Resize {
                        rows,
                        cols,
                        pixel_width,
                        pixel_height,
                    }) => {
                        shared.resize_emulator(rows, cols, pixel_width, pixel_height);
                        shared.bump_revision();
                    }
                    Ok(AttachFrame::Reconnect) => {
                        let _ = AttachFrame::ReconnectAck.write_to(&mut stream);
                        shared.set_status("reconnecting");
                        let profile = shared.profile.lock().unwrap().clone();
                        match reconnect(&client, &shell_id, expected_run_id.as_deref(), &profile) {
                            Ok(attachment) => {
                                stream = attachment.stream;
                                if let Err(error) = resynchronize_terminal_size(
                                    &mut stream,
                                    profile.rows,
                                    profile.cols,
                                    profile.pixel_width,
                                    profile.pixel_height,
                                ) {
                                    shared
                                        .close(format!("could not restore terminal size: {error}"));
                                    return;
                                }
                                if let Err(error) = shared.install_writer(&stream) {
                                    shared.close(error);
                                    return;
                                }
                                shared.process(attachment.reconstruction);
                                shared.set_status("attached");
                            }
                            Err(error) => {
                                shared.close(error);
                                return;
                            }
                        }
                    }
                    Ok(AttachFrame::Detached) => {
                        shared.close("detached");
                        return;
                    }
                    Ok(_) => {
                        shared.close("Boomux sent an invalid terminal frame");
                        return;
                    }
                    Err(error) if error.kind() == ErrorKind::UnexpectedEof => {
                        shared.close("connection closed");
                        return;
                    }
                    Err(error) => {
                        shared.close(format!("terminal read failed: {error}"));
                        return;
                    }
                }
            }
        })
        .expect("spawn Boomux terminal reader");
}

fn resynchronize_terminal_size(
    stream: &mut std::os::unix::net::UnixStream,
    rows: u16,
    cols: u16,
    pixel_width: u16,
    pixel_height: u16,
) -> Result<(), String> {
    let (nudged_rows, nudged_cols) = if rows > 1 {
        (rows - 1, cols)
    } else if cols > 1 {
        (rows, cols - 1)
    } else {
        return Ok(());
    };
    AttachFrame::Resize {
        rows: nudged_rows,
        cols: nudged_cols,
        pixel_width,
        pixel_height,
    }
    .write_to(stream)
    .map_err(|error| format!("could not nudge terminal size: {error}"))?;
    thread::sleep(RESIZE_SETTLE);
    AttachFrame::Resize {
        rows,
        cols,
        pixel_width,
        pixel_height,
    }
    .write_to(stream)
    .map_err(|error| format!("could not synchronize terminal size: {error}"))
}

fn reconnect(
    client: &Client,
    shell_id: &str,
    expected_run_id: Option<&str>,
    profile: &TerminalProfile,
) -> Result<client::Attachment, String> {
    let mut last_error = None;
    for _ in 0..RECONNECT_ATTEMPTS {
        let result = if let Some(run_id) = expected_run_id {
            client.attach_exact_run(shell_id, run_id, false, profile.clone())
        } else {
            client.attach(shell_id, false, profile.clone())
        };
        match result {
            Ok(attachment) => return Ok(attachment),
            Err(error) => last_error = Some(error),
        }
        thread::sleep(RECONNECT_DELAY);
    }
    Err(format!(
        "could not reconnect Boomux terminal: {}",
        last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| "unknown error".into())
    ))
}

fn encode_key(
    terminal: &GhosttyTerminal<'_, '_>,
    encoder: &mut KeyEncoder<'_>,
    keystroke: &Keystroke,
    action: KeyAction,
) -> Result<Vec<u8>, String> {
    // Boomux's terminal reconstruction does not currently preserve Kitty
    // keyboard flags. An enhanced TUI reattached after its negotiation can
    // therefore receive modifyOtherKeys for Shift+Enter even though it expects
    // Kitty CSI-u. LF is the portable Ctrl+J spelling that Codex and other
    // readline-style editors already treat as an inserted newline. It is also
    // harmless in applications that do not distinguish Shift+Enter.
    if keystroke.key == "enter"
        && keystroke.modifiers.shift
        && !keystroke.modifiers.control
        && !keystroke.modifiers.alt
        && !keystroke.modifiers.platform
    {
        return Ok(if matches!(action, KeyAction::Press | KeyAction::Repeat) {
            b"\n".to_vec()
        } else {
            Vec::new()
        });
    }

    let (unshifted_key, implied_shift) = unshifted_key(&keystroke.key);
    let key = ghostty_key(unshifted_key).unwrap_or(GhosttyKey::Unidentified);
    let text = keystroke
        .key_char
        .as_deref()
        .filter(|text| valid_key_text(text));
    let mut mods = GhosttyMods::empty();
    mods.set(
        GhosttyMods::SHIFT,
        keystroke.modifiers.shift || implied_shift,
    );
    mods.set(GhosttyMods::ALT, keystroke.modifiers.alt);
    mods.set(GhosttyMods::CTRL, keystroke.modifiers.control);
    mods.set(GhosttyMods::SUPER, keystroke.modifiers.platform);
    let mut consumed_mods = GhosttyMods::empty();
    consumed_mods.set(
        GhosttyMods::SHIFT,
        text.is_some() && mods.contains(GhosttyMods::SHIFT),
    );

    let mut event =
        KeyEvent::new().map_err(|error| format!("could not create Ghostty key event: {error}"))?;
    event
        .set_action(action)
        .set_key(key)
        .set_mods(mods)
        .set_consumed_mods(consumed_mods);
    if let Some(text) = text {
        event.set_utf8(Some(text));
    }
    if let Some(unshifted) = unshifted_codepoint(unshifted_key) {
        event.set_unshifted_codepoint(unshifted);
    }

    encoder.set_options_from_terminal(terminal);
    let mut output = [0_u8; 128];
    let written = encoder
        .encode(&event, &mut output)
        .map_err(|error| format!("could not encode Ghostty key event: {error}"))?;
    Ok(output[..written].to_vec())
}

fn terminal_key_supported(keystroke: &Keystroke) -> bool {
    !keystroke.modifiers.function
        && (ghostty_key(unshifted_key(&keystroke.key).0).is_some()
            || keystroke.key_char.as_deref().is_some_and(valid_key_text))
}

fn valid_key_text(text: &str) -> bool {
    !text.is_empty()
        && !text.chars().any(|character| {
            character.is_control() || ('\u{f700}'..='\u{f8ff}').contains(&character)
        })
}

fn unshifted_key(key: &str) -> (&str, bool) {
    match key {
        "!" => ("1", true),
        "@" => ("2", true),
        "#" => ("3", true),
        "$" => ("4", true),
        "%" => ("5", true),
        "^" => ("6", true),
        "&" => ("7", true),
        "*" => ("8", true),
        "(" => ("9", true),
        ")" => ("0", true),
        "_" => ("-", true),
        "+" => ("=", true),
        "{" => ("[", true),
        "}" => ("]", true),
        "|" => ("\\", true),
        ":" => (";", true),
        "\"" => ("'", true),
        "<" => (",", true),
        ">" => (".", true),
        "?" => ("/", true),
        "~" => ("`", true),
        _ => (key, false),
    }
}

fn unshifted_codepoint(key: &str) -> Option<char> {
    if key == "space" {
        return Some(' ');
    }
    let mut characters = key.chars();
    let character = characters.next()?;
    characters.next().is_none().then_some(character)
}

fn ghostty_key(key: &str) -> Option<GhosttyKey> {
    Some(match key {
        "`" => GhosttyKey::Backquote,
        "\\" => GhosttyKey::Backslash,
        "[" => GhosttyKey::BracketLeft,
        "]" => GhosttyKey::BracketRight,
        "," => GhosttyKey::Comma,
        "0" => GhosttyKey::Digit0,
        "1" => GhosttyKey::Digit1,
        "2" => GhosttyKey::Digit2,
        "3" => GhosttyKey::Digit3,
        "4" => GhosttyKey::Digit4,
        "5" => GhosttyKey::Digit5,
        "6" => GhosttyKey::Digit6,
        "7" => GhosttyKey::Digit7,
        "8" => GhosttyKey::Digit8,
        "9" => GhosttyKey::Digit9,
        "=" => GhosttyKey::Equal,
        "a" => GhosttyKey::A,
        "b" => GhosttyKey::B,
        "c" => GhosttyKey::C,
        "d" => GhosttyKey::D,
        "e" => GhosttyKey::E,
        "f" => GhosttyKey::F,
        "g" => GhosttyKey::G,
        "h" => GhosttyKey::H,
        "i" => GhosttyKey::I,
        "j" => GhosttyKey::J,
        "k" => GhosttyKey::K,
        "l" => GhosttyKey::L,
        "m" => GhosttyKey::M,
        "n" => GhosttyKey::N,
        "o" => GhosttyKey::O,
        "p" => GhosttyKey::P,
        "q" => GhosttyKey::Q,
        "r" => GhosttyKey::R,
        "s" => GhosttyKey::S,
        "t" => GhosttyKey::T,
        "u" => GhosttyKey::U,
        "v" => GhosttyKey::V,
        "w" => GhosttyKey::W,
        "x" => GhosttyKey::X,
        "y" => GhosttyKey::Y,
        "z" => GhosttyKey::Z,
        "-" => GhosttyKey::Minus,
        "." => GhosttyKey::Period,
        "'" => GhosttyKey::Quote,
        ";" => GhosttyKey::Semicolon,
        "/" => GhosttyKey::Slash,
        "backspace" => GhosttyKey::Backspace,
        "enter" | "return" => GhosttyKey::Enter,
        "space" => GhosttyKey::Space,
        "tab" => GhosttyKey::Tab,
        "delete" => GhosttyKey::Delete,
        "end" => GhosttyKey::End,
        "home" => GhosttyKey::Home,
        "insert" => GhosttyKey::Insert,
        "pagedown" | "page_down" | "page-down" => GhosttyKey::PageDown,
        "pageup" | "page_up" | "page-up" => GhosttyKey::PageUp,
        "down" => GhosttyKey::ArrowDown,
        "left" => GhosttyKey::ArrowLeft,
        "right" => GhosttyKey::ArrowRight,
        "up" => GhosttyKey::ArrowUp,
        "add" => GhosttyKey::NumpadAdd,
        "begin" => GhosttyKey::NumpadBegin,
        "clear" => GhosttyKey::NumpadClear,
        "decimal" => GhosttyKey::NumpadDecimal,
        "divide" => GhosttyKey::NumpadDivide,
        "equal" => GhosttyKey::NumpadEqual,
        "multiply" => GhosttyKey::NumpadMultiply,
        "separator" => GhosttyKey::NumpadSeparator,
        "subtract" => GhosttyKey::NumpadSubtract,
        "escape" => GhosttyKey::Escape,
        "f1" => GhosttyKey::F1,
        "f2" => GhosttyKey::F2,
        "f3" => GhosttyKey::F3,
        "f4" => GhosttyKey::F4,
        "f5" => GhosttyKey::F5,
        "f6" => GhosttyKey::F6,
        "f7" => GhosttyKey::F7,
        "f8" => GhosttyKey::F8,
        "f9" => GhosttyKey::F9,
        "f10" => GhosttyKey::F10,
        "f11" => GhosttyKey::F11,
        "f12" => GhosttyKey::F12,
        "f13" => GhosttyKey::F13,
        "f14" => GhosttyKey::F14,
        "f15" => GhosttyKey::F15,
        "f16" => GhosttyKey::F16,
        "f17" => GhosttyKey::F17,
        "f18" => GhosttyKey::F18,
        "f19" => GhosttyKey::F19,
        "f20" => GhosttyKey::F20,
        "f21" => GhosttyKey::F21,
        "f22" => GhosttyKey::F22,
        "f23" => GhosttyKey::F23,
        "f24" => GhosttyKey::F24,
        "f25" => GhosttyKey::F25,
        _ => return None,
    })
}

#[cfg(test)]
fn indexed_color(index: u8) -> u32 {
    indexed_color_with_palette(index, &crate::theme::current_terminal().ansi)
}

fn indexed_color_with_palette(index: u8, ansi: &[u32; 16]) -> u32 {
    match index {
        0..=15 => ansi[usize::from(index)],
        16..=231 => {
            let value = index - 16;
            let component = |part: u8| if part == 0 { 0 } else { 55 + part * 40 };
            let red = component(value / 36);
            let green = component((value % 36) / 6);
            let blue = component(value % 6);
            (u32::from(red) << 16) | (u32::from(green) << 8) | u32::from(blue)
        }
        232..=255 => {
            let gray = 8 + (index - 232) * 10;
            (u32::from(gray) << 16) | (u32::from(gray) << 8) | u32::from(gray)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::Ordering;
    use std::thread;
    use std::time::Duration;

    use boomux::protocol::{AgentState, AttachFrame};
    use gpui::{Keystroke, Modifiers};
    use libghostty_vt::key::Action as KeyAction;
    use libghostty_vt::terminal::Mode;

    use super::{
        EmulatorCommand, EmulatorCore, SharedTerminal, agent_is_visible, blank_screen,
        configure_terminal, encode_key, encode_mouse_wheel, encode_paste, image_bgra,
        indexed_color, resynchronize_terminal_size, terminal_profile,
    };
    use crate::theme::TerminalTheme;
    use std::sync::Arc;

    fn key(key: &str, key_char: Option<&str>, modifiers: Modifiers) -> Keystroke {
        Keystroke {
            key: key.into(),
            key_char: key_char.map(str::to_owned),
            modifiers,
        }
    }

    #[test]
    fn agent_visibility_requires_a_current_shell_run_or_attention() {
        assert!(agent_is_visible(AgentState::Idle, false, true));
        assert!(!agent_is_visible(AgentState::Idle, false, false));
        assert!(!agent_is_visible(AgentState::Inactive, false, true));
        assert!(!agent_is_visible(AgentState::Done, false, true));
        assert!(agent_is_visible(AgentState::Done, true, false));
    }

    #[test]
    fn terminal_profile_uses_a_portable_term_type() {
        let profile = terminal_profile(24, 80, 800, 480);

        assert_eq!(profile.term.as_deref(), Some("xterm-256color"));
        assert_eq!(profile.colorterm.as_deref(), Some("truecolor"));
    }

    #[test]
    fn absolute_scroll_requests_keep_only_the_latest_row() {
        let shared = SharedTerminal::new(terminal_profile(24, 80, 800, 480));
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        shared.install_emulator(sender);

        shared.scroll_to_row(10).unwrap();
        shared.scroll_to_row(40).unwrap();
        shared.scroll_to_row(75).unwrap();

        assert!(matches!(
            receiver.try_recv(),
            Ok(EmulatorCommand::ScrollLatest)
        ));
        assert!(receiver.try_recv().is_err());
        assert_eq!(shared.pending_scroll_row.load(Ordering::Acquire), 75);
        assert!(shared.pending_scroll_wakeup.load(Ordering::Acquire));
    }

    #[test]
    fn rejected_scroll_request_releases_its_wakeup_slot() {
        let shared = SharedTerminal::new(terminal_profile(24, 80, 800, 480));
        assert!(shared.scroll_to_row(10).is_err());
        assert!(!shared.pending_scroll_wakeup.load(Ordering::Acquire));
    }

    #[test]
    fn terminal_key_submission_is_bounded_and_nonblocking() {
        let shared = SharedTerminal::new(terminal_profile(24, 80, 800, 480));
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        shared.install_emulator(sender);
        shared
            .emulator_command(EmulatorCommand::Output(Vec::new()))
            .unwrap();

        assert_eq!(
            shared
                .try_key_command(key("a", Some("a"), Modifiers::default()), KeyAction::Press,)
                .unwrap_err(),
            "terminal input queue is full"
        );
        assert!(matches!(
            receiver.try_recv(),
            Ok(EmulatorCommand::Output(_))
        ));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn terminal_theme_requests_are_bounded_and_keep_the_latest_palette() {
        let shared = SharedTerminal::new(terminal_profile(24, 80, 800, 480));
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        shared.install_emulator(sender);
        let first = TerminalTheme {
            foreground: 0x111111,
            background: 0x222222,
            cursor: 0x333333,
            ansi: [0x444444; 16],
        };
        let latest = TerminalTheme {
            foreground: 0xaaaaaa,
            background: 0xbbbbbb,
            cursor: 0xcccccc,
            ansi: [0xdddddd; 16],
        };

        shared.set_theme(first).unwrap();
        shared.set_theme(latest).unwrap();

        assert!(matches!(
            receiver.try_recv(),
            Ok(EmulatorCommand::ThemeLatest)
        ));
        assert!(receiver.try_recv().is_err());
        assert_eq!(*shared.pending_theme.lock().unwrap(), Some(latest));
    }

    #[test]
    fn terminal_update_events_are_bounded_and_coalesced() {
        let shared = SharedTerminal::new(terminal_profile(24, 80, 800, 480));
        let events = shared.update_events.clone();

        shared.bump_revision();
        shared.bump_revision();
        shared.bump_revision();

        assert!(events.try_recv().is_ok());
        assert!(events.try_recv().is_err());
        assert_eq!(shared.revision.load(Ordering::Acquire), 4);
    }

    #[test]
    fn terminal_viewport_bottom_detection_uses_the_latest_snapshot() {
        let shared = SharedTerminal::new(terminal_profile(24, 80, 800, 480));
        let mut screen = blank_screen(24, 80);
        screen.scroll_total = 100;
        screen.scroll_len = 24;
        screen.scroll_offset = 76;
        *shared.screen.lock().unwrap() = Arc::new(screen.clone());
        assert!(shared.viewport_is_at_bottom());

        screen.scroll_offset = 75;
        *shared.screen.lock().unwrap() = Arc::new(screen);
        assert!(!shared.viewport_is_at_bottom());
    }

    #[test]
    fn encodes_legacy_text_control_and_negotiated_cursor_keys() {
        let shared = Arc::new(SharedTerminal::new(terminal_profile(24, 80, 800, 480)));
        let mut core = EmulatorCore::new(&shared, 24, 80, 800, 480).unwrap();
        let EmulatorCore {
            terminal,
            key: encoder,
            ..
        } = &mut core;

        assert_eq!(
            encode_key(
                terminal,
                encoder,
                &key("a", Some("a"), Modifiers::default()),
                KeyAction::Press,
            )
            .unwrap(),
            b"a"
        );
        assert_eq!(
            encode_key(
                terminal,
                encoder,
                &key(
                    "c",
                    None,
                    Modifiers {
                        control: true,
                        ..Default::default()
                    }
                ),
                KeyAction::Press,
            ),
            Ok(vec![3])
        );
        assert_eq!(
            encode_key(
                terminal,
                encoder,
                &key(
                    "a",
                    Some("a"),
                    Modifiers {
                        alt: true,
                        ..Default::default()
                    },
                ),
                KeyAction::Press,
            )
            .unwrap(),
            b"\x1ba"
        );
        assert_eq!(
            encode_key(
                terminal,
                encoder,
                &key("!", Some("!"), Modifiers::default()),
                KeyAction::Press,
            )
            .unwrap(),
            b"!"
        );
        assert_eq!(
            encode_key(
                terminal,
                encoder,
                &key("é", Some("é"), Modifiers::default()),
                KeyAction::Press,
            )
            .unwrap(),
            "é".as_bytes()
        );
        assert_eq!(
            encode_key(
                terminal,
                encoder,
                &key("up", None, Modifiers::default()),
                KeyAction::Press,
            )
            .unwrap(),
            b"\x1b[A"
        );
        terminal.vt_write(b"\x1b[?1h");
        assert_eq!(
            encode_key(
                terminal,
                encoder,
                &key("up", None, Modifiers::default()),
                KeyAction::Press,
            )
            .unwrap(),
            b"\x1bOA"
        );
        assert_eq!(
            encode_key(
                terminal,
                encoder,
                &key(
                    "up",
                    None,
                    Modifiers {
                        control: true,
                        ..Default::default()
                    },
                ),
                KeyAction::Press,
            )
            .unwrap(),
            b"\x1b[1;5A"
        );
        assert_eq!(
            encode_key(
                terminal,
                encoder,
                &key("f5", None, Modifiers::default()),
                KeyAction::Press,
            )
            .unwrap(),
            b"\x1b[15~"
        );
        assert_eq!(
            encode_key(
                terminal,
                encoder,
                &key("add", Some("+"), Modifiers::default()),
                KeyAction::Press,
            )
            .unwrap(),
            b"+"
        );
        terminal.vt_write(b"\x1b[?1035l\x1b[?66h");
        assert_eq!(
            encode_key(
                terminal,
                encoder,
                &key("add", Some("+"), Modifiers::default()),
                KeyAction::Press,
            )
            .unwrap(),
            b"\x1bOk"
        );
        assert_eq!(
            encode_key(
                terminal,
                encoder,
                &key(
                    "tab",
                    None,
                    Modifiers {
                        shift: true,
                        ..Default::default()
                    },
                ),
                KeyAction::Press,
            )
            .unwrap(),
            b"\x1b[Z"
        );
    }

    #[test]
    fn shift_enter_inserts_a_newline_across_keyboard_modes() {
        let shared = Arc::new(SharedTerminal::new(terminal_profile(24, 80, 800, 480)));
        let mut core = EmulatorCore::new(&shared, 24, 80, 800, 480).unwrap();
        let EmulatorCore {
            terminal,
            key: encoder,
            ..
        } = &mut core;
        let enter = key("enter", None, Modifiers::default());
        let shift_enter = key(
            "enter",
            None,
            Modifiers {
                shift: true,
                ..Default::default()
            },
        );

        assert_eq!(
            encode_key(terminal, encoder, &enter, KeyAction::Press).unwrap(),
            b"\r"
        );
        assert_eq!(
            encode_key(terminal, encoder, &shift_enter, KeyAction::Press).unwrap(),
            b"\n"
        );

        terminal.vt_write(b"\x1b[>1u");
        assert_eq!(
            encode_key(terminal, encoder, &enter, KeyAction::Press).unwrap(),
            b"\r"
        );
        assert_eq!(
            encode_key(terminal, encoder, &shift_enter, KeyAction::Press).unwrap(),
            b"\n"
        );

        terminal.vt_write(b"\x1b[>11u");
        assert_eq!(
            encode_key(terminal, encoder, &shift_enter, KeyAction::Repeat).unwrap(),
            b"\n"
        );
        assert_eq!(
            encode_key(terminal, encoder, &shift_enter, KeyAction::Release).unwrap(),
            b""
        );

        terminal.vt_write(b"\x1b[<u\x1b[>7u");
        assert_eq!(
            encode_key(terminal, encoder, &shift_enter, KeyAction::Press).unwrap(),
            b"\n"
        );
    }

    #[test]
    fn paste_normalizes_newlines_and_honors_bracketed_mode() {
        assert_eq!(encode_paste("one\r\ntwo\r", false), b"one\ntwo\n");
        assert_eq!(
            encode_paste("one\r\ntwo", true),
            b"\x1b[200~one\ntwo\x1b[201~"
        );
    }

    #[test]
    fn mouse_wheel_uses_the_tuis_negotiated_protocol() {
        let shared = Arc::new(SharedTerminal::new(terminal_profile(24, 80, 800, 480)));
        let mut core = EmulatorCore::new(&shared, 24, 80, 800, 480).unwrap();
        core.apply(EmulatorCommand::Output(b"\x1b[?1000h\x1b[?1006h".to_vec()))
            .unwrap();
        let EmulatorCore {
            terminal, mouse, ..
        } = &mut core;
        let bytes = encode_mouse_wheel(
            terminal,
            mouse,
            2,
            (24.0, 30.0),
            (800, 480),
            10,
            20,
            Modifiers::default(),
        )
        .unwrap();

        assert_eq!(bytes.iter().filter(|byte| **byte == 0x1b).count(), 2);
        assert!(bytes.starts_with(b"\x1b[<64;"));

        terminal.vt_write(b"\x1b[?1000l\x1b[?1006l");
        assert!(
            encode_mouse_wheel(
                terminal,
                mouse,
                -1,
                (24.0, 30.0),
                (800, 480),
                10,
                20,
                Modifiers::default(),
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn maps_256_color_cube_and_grayscale() {
        assert_eq!(indexed_color(16), 0x000000);
        assert_eq!(indexed_color(231), 0xffffff);
        assert_eq!(indexed_color(232), 0x080808);
        assert_eq!(indexed_color(255), 0xeeeeee);
    }

    #[test]
    fn terminal_palette_updates_existing_default_cells() {
        let shared = Arc::new(SharedTerminal::new(terminal_profile(2, 10, 100, 40)));
        let mut core = EmulatorCore::new(&shared, 2, 10, 100, 40).unwrap();
        let theme = TerminalTheme {
            foreground: 0xabcdef,
            background: 0x123456,
            cursor: 0xfedcba,
            ansi: [0x010203; 16],
        };
        configure_terminal(&mut core.terminal, theme).unwrap();
        let screen = core.screen().unwrap();
        assert_eq!(screen.cells[0].foreground, theme.foreground);
        assert_eq!(screen.cells[0].background, theme.background);
    }

    #[test]
    fn ghostty_reflows_wrapped_content_when_resized() {
        let shared = Arc::new(SharedTerminal::new(terminal_profile(2, 10, 100, 40)));
        let mut core = EmulatorCore::new(&shared, 2, 10, 100, 40).unwrap();
        core.apply(EmulatorCommand::Output(b"abcdefghijklmnop".to_vec()))
            .unwrap();
        core.apply(EmulatorCommand::Resize {
            rows: 4,
            cols: 5,
            cell_width: 10,
            cell_height: 20,
        })
        .unwrap();

        let screen = core.screen().unwrap();
        let lines = screen
            .cells
            .chunks(usize::from(screen.cols))
            .map(|row| {
                row.iter()
                    .map(|cell| cell.text.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert_eq!(&lines[..3], ["abcde", "fghij", "klmno"]);
        assert!(lines[3].starts_with('p'));
    }

    #[test]
    fn ghostty_retains_history_before_and_after_pane_resize() {
        for cols in [80, 160, 240] {
            let shared = Arc::new(SharedTerminal::new(terminal_profile(
                40,
                cols,
                cols * 10,
                800,
            )));
            let mut core = EmulatorCore::new(&shared, 40, cols, cols * 10, 800).unwrap();
            let output = (0..500)
                .map(|line| format!("history-{line:04}\r\n"))
                .collect::<String>();
            core.apply(EmulatorCommand::Output(output.into_bytes()))
                .unwrap();
            for width in [cols, cols / 2, cols] {
                core.apply(EmulatorCommand::Resize {
                    rows: 40,
                    cols: width,
                    cell_width: 10,
                    cell_height: 20,
                })
                .unwrap();
                core.apply(EmulatorCommand::Scroll(
                    libghostty_vt::terminal::ScrollViewport::Top,
                ))
                .unwrap();
                let screen = core.screen().unwrap();
                assert!(
                    screen.scroll_total >= 501,
                    "{cols} initial columns, {width} current: only {} total rows retained",
                    screen.scroll_total
                );
                let first_row = screen.cells[..usize::from(screen.cols)]
                    .iter()
                    .map(|cell| cell.text.as_str())
                    .collect::<String>();
                assert!(first_row.starts_with("history-0000"), "{first_row:?}");
            }
        }
    }

    #[test]
    fn ghostty_scrollback_prunes_old_output_when_the_budget_is_full() {
        let shared = Arc::new(SharedTerminal::new(terminal_profile(40, 160, 1600, 800)));
        let mut core = EmulatorCore::new(&shared, 40, 160, 1600, 800).unwrap();
        let output = (0..20_000)
            .map(|line| format!("history-{line:05}\r\n"))
            .collect::<String>();
        core.apply(EmulatorCommand::Output(output.into_bytes()))
            .unwrap();
        core.apply(EmulatorCommand::Scroll(
            libghostty_vt::terminal::ScrollViewport::Top,
        ))
        .unwrap();
        let oldest = core.screen().unwrap();
        assert!(oldest.scroll_total > 500);
        assert!(
            oldest.scroll_total < 20_001,
            "history must not grow without a bound"
        );
        let first_row = oldest.cells[..usize::from(oldest.cols)]
            .iter()
            .map(|cell| cell.text.as_str())
            .collect::<String>();
        assert!(
            !first_row.starts_with("history-00000"),
            "old output must be pruned"
        );
        core.apply(EmulatorCommand::Scroll(
            libghostty_vt::terminal::ScrollViewport::Bottom,
        ))
        .unwrap();
        let newest = core.screen().unwrap();
        let text = newest
            .cells
            .iter()
            .map(|cell| cell.text.as_str())
            .collect::<String>();
        assert!(
            text.contains("history-19999"),
            "newest output must remain available"
        );
    }

    #[test]
    fn ghostty_scrolls_history_and_returns_to_bottom() {
        let shared = Arc::new(SharedTerminal::new(terminal_profile(3, 10, 100, 60)));
        let mut core = EmulatorCore::new(&shared, 3, 10, 100, 60).unwrap();
        core.apply(EmulatorCommand::Output(
            b"one\r\ntwo\r\nthree\r\nfour\r\nfive".to_vec(),
        ))
        .unwrap();
        let bottom = core.screen().unwrap();
        assert_eq!(
            bottom.scroll_offset,
            bottom.scroll_total.saturating_sub(bottom.scroll_len)
        );

        core.apply(EmulatorCommand::Scroll(
            libghostty_vt::terminal::ScrollViewport::Delta(-2),
        ))
        .unwrap();
        let history = core.screen().unwrap();
        let history_text = history
            .cells
            .iter()
            .map(|cell| cell.text.as_str())
            .collect::<String>();
        assert!(history_text.contains("one"), "{history_text:?}");
        assert_ne!(history, bottom);
        assert!(history.scroll_offset < bottom.scroll_offset);

        core.apply(EmulatorCommand::Scroll(
            libghostty_vt::terminal::ScrollViewport::Bottom,
        ))
        .unwrap();
        assert_eq!(core.screen().unwrap(), bottom);
    }

    #[test]
    fn ghostty_decodes_and_places_kitty_rgb_images() {
        let shared = Arc::new(SharedTerminal::new(terminal_profile(3, 10, 100, 60)));
        let mut core = EmulatorCore::new(&shared, 3, 10, 100, 60).unwrap();
        core.apply(EmulatorCommand::Output(
            b"\x1b_Gf=24,s=2,v=1,i=7;/wAAAP8A\x1b\\\x1b_Ga=p,i=7,c=2,r=1,C=1\x1b\\".to_vec(),
        ))
        .unwrap();

        let screen = core.screen().unwrap();
        assert_eq!(screen.images.len(), 1);
        assert_eq!(screen.images[0].width, 2);
        assert_eq!(screen.images[0].height, 1);
        assert_eq!(
            screen.images[0].bgra.as_ref(),
            &[0, 0, 255, 255, 0, 255, 0, 255]
        );
        assert_eq!(screen.image_placements.len(), 1);
        assert_eq!(screen.image_placements[0].pixel_width, 20);
        assert_eq!(screen.image_placements[0].pixel_height, 20);

        let unchanged = core.screen().unwrap();
        assert!(Arc::ptr_eq(
            &screen.images[0].bgra,
            &unchanged.images[0].bgra
        ));
    }

    #[test]
    fn ghostty_advertises_kitty_graphics_to_terminal_apps() {
        let (client, mut daemon) = UnixStream::pair().unwrap();
        daemon
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let shared = Arc::new(SharedTerminal::new(terminal_profile(3, 10, 100, 60)));
        shared.install_writer(&client).unwrap();
        let mut core = EmulatorCore::new(&shared, 3, 10, 100, 60).unwrap();
        core.apply(EmulatorCommand::Output(b"\x1b_Gi=1,a=q\x1b\\".to_vec()))
            .unwrap();

        let response = AttachFrame::read_from(&mut daemon).unwrap();
        let AttachFrame::Input(bytes) = response else {
            panic!("expected a Kitty graphics capability response");
        };
        assert_eq!(bytes, b"\x1b_Gi=1;OK\x1b\\");
    }

    #[test]
    fn ghostty_answers_keyboard_enhancement_probe() {
        let (client, mut daemon) = UnixStream::pair().unwrap();
        daemon
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let shared = Arc::new(SharedTerminal::new(terminal_profile(3, 10, 100, 60)));
        shared.install_writer(&client).unwrap();
        let mut core = EmulatorCore::new(&shared, 3, 10, 100, 60).unwrap();

        core.apply(EmulatorCommand::Output(b"\x1b[?u".to_vec()))
            .unwrap();

        let response = AttachFrame::read_from(&mut daemon).unwrap();
        let AttachFrame::Input(bytes) = response else {
            panic!("expected a keyboard enhancement response");
        };
        assert_eq!(bytes, b"\x1b[?0u");
    }

    #[test]
    fn ghostty_accepts_a_doom_sized_synchronized_frame() {
        const WIDTH: usize = 640;
        const HEIGHT: usize = 400;
        const ENCODED_BYTES: usize = WIDTH * HEIGHT * 4;

        let shared = Arc::new(SharedTerminal::new(terminal_profile(30, 80, 800, 600)));
        let mut core = EmulatorCore::new(&shared, 30, 80, 800, 600).unwrap();
        let mut output = b"\x1b[?2026h\x1b_Ga=d\x1b\\".to_vec();
        for offset in (0..ENCODED_BYTES).step_by(4096) {
            let first = offset == 0;
            let final_chunk = offset + 4096 == ENCODED_BYTES;
            if first {
                output.extend_from_slice(b"\x1b_Gf=24,s=640,v=400,i=1,m=1;");
            } else if final_chunk {
                output.extend_from_slice(b"\x1b_Gm=0;");
            } else {
                output.extend_from_slice(b"\x1b_Gm=1;");
            }
            output.extend(std::iter::repeat_n(b'A', 4096));
            output.extend_from_slice(b"\x1b\\");
        }
        output.extend_from_slice(b"\x1b_Ga=p,i=1,c=64,r=24,C=1\x1b\\\x1b[?2026l");

        core.apply(EmulatorCommand::Output(output)).unwrap();
        assert!(!core.terminal.mode(Mode::SYNC_OUTPUT).unwrap());
        let screen = core.screen().unwrap();
        assert_eq!(screen.images.len(), 1);
        assert_eq!(screen.images[0].bgra.len(), WIDTH * HEIGHT * 4);
        assert_eq!(screen.image_placements.len(), 1);
        assert_eq!(screen.image_placements[0].pixel_width, 640);
        assert_eq!(screen.image_placements[0].pixel_height, 480);
    }

    #[test]
    fn converts_supported_kitty_formats_to_bgra() {
        assert_eq!(
            image_bgra(
                libghostty_vt::kitty::graphics::ImageFormat::Rgb,
                1,
                1,
                &[1, 2, 3]
            )
            .unwrap(),
            [3, 2, 1, 255]
        );
        assert_eq!(
            image_bgra(
                libghostty_vt::kitty::graphics::ImageFormat::GrayAlpha,
                1,
                1,
                &[9, 10]
            )
            .unwrap(),
            [9, 9, 9, 10]
        );
    }

    #[test]
    fn attachment_resize_is_nudged_then_restored() {
        let (mut client, mut daemon) = UnixStream::pair().unwrap();
        let receiver = thread::spawn(move || {
            assert_eq!(
                AttachFrame::read_from(&mut daemon).unwrap(),
                AttachFrame::Resize {
                    rows: 21,
                    cols: 70,
                    pixel_width: 588,
                    pixel_height: 374,
                }
            );
            assert_eq!(
                AttachFrame::read_from(&mut daemon).unwrap(),
                AttachFrame::Resize {
                    rows: 22,
                    cols: 70,
                    pixel_width: 588,
                    pixel_height: 374,
                }
            );
        });

        resynchronize_terminal_size(&mut client, 22, 70, 588, 374).unwrap();
        receiver.join().unwrap();
    }
}
