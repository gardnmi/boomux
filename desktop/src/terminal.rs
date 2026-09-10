use std::collections::{HashMap, HashSet};
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use boomux::client::{self, Client};
use boomux::protocol::{
    AgentAttentionReason, AgentState, AttachFrame, ErrorCode, Request, Response, ShellSnapshot,
    ShellSpec, ShellStatus, TerminalProfile, WorkspaceSnapshot,
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
    pub desktop_setup: bool,
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
    pub run_id: String,
    pub shell_name: String,
    pub display_name: String,
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
    pending_resize: Mutex<Option<(u16, u16, u16, u16)>>,
    pending_focus: AtomicBool,
    pending_scroll_row: AtomicU64,
    pending_scroll_wakeup: AtomicBool,
    pending_theme: Mutex<Option<TerminalTheme>>,
    closed: AtomicBool,
    cancelled: AtomicBool,
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
            pending_resize: Mutex::new(None),
            pending_focus: AtomicBool::new(false),
            pending_scroll_row: AtomicU64::new(0),
            pending_scroll_wakeup: AtomicBool::new(false),
            pending_theme: Mutex::new(Some(theme)),
            closed: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
        }
    }

    fn install_emulator(&self, sender: mpsc::SyncSender<EmulatorCommand>) {
        *self.emulator.lock().unwrap() = Some(sender);
    }

    fn emulator_command(&self, command: EmulatorCommand) -> Result<(), String> {
        // Output producers may wait for queue capacity, but must never keep
        // the sender mutex locked while waiting: UI input and teardown use it.
        let sender = self
            .emulator
            .lock()
            .unwrap()
            .as_ref()
            .cloned()
            .ok_or_else(|| "Ghostty terminal core is not running".to_string())?;
        sender
            .send(command)
            .map_err(|_| "Ghostty terminal core stopped".to_string())
    }

    fn cancel_emulator(&self) {
        self.cancelled.store(true, Ordering::Release);
        // Disconnect instead of enqueueing Stop into a potentially full queue.
        // The worker sees cancellation between replay chunks and drops its
        // receiver, releasing any blocked output producer.
        self.emulator.lock().unwrap().take();
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

    fn request_resize(&self, size: (u16, u16, u16, u16)) -> Result<(), String> {
        let wake = self.pending_resize.lock().unwrap().replace(size).is_none();
        if wake {
            self.try_emulator_command(EmulatorCommand::ResizeLatest)?;
        }
        Ok(())
    }

    fn flush_pending_resize(&self, core: &mut EmulatorCore) -> Result<(), String> {
        let pending = self.pending_resize.lock().unwrap().take();
        if let Some((rows, cols, pixel_width, pixel_height)) = pending {
            {
                let mut profile = self.profile.lock().unwrap();
                profile.rows = rows;
                profile.cols = cols;
                profile.pixel_width = pixel_width;
                profile.pixel_height = pixel_height;
            }
            core.apply(EmulatorCommand::Resize {
                rows,
                cols,
                cell_width: cell_dimension(pixel_width, cols),
                cell_height: cell_dimension(pixel_height, rows),
            })?;
            self.send(AttachFrame::Resize {
                rows,
                cols,
                pixel_width,
                pixel_height,
            })?;
            self.bump_revision();
        }
        Ok(())
    }

    fn request_focus(&self) -> Result<(), String> {
        if self.pending_focus.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        // A full queue already wakes the worker. The pending flag retains the
        // notification until that worker can write it, without blocking GPUI.
        self.try_emulator_command(EmulatorCommand::FocusLatest)
    }

    fn flush_pending_focus(&self) -> Result<(), String> {
        if self.pending_focus.swap(false, Ordering::AcqRel) {
            self.send(AttachFrame::FocusGained)?;
        }
        Ok(())
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
        // Transport closure still drains queued output and publishes the final
        // screen. Disconnecting the sender does not require queue capacity.
        self.emulator.lock().unwrap().take();
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
    FocusLatest,
    ResizeLatest,
    MouseWheel {
        lines: isize,
        x: f32,
        y: f32,
        screen_width: u32,
        screen_height: u32,
        modifiers: Modifiers,
    },
}

pub struct TerminalSession {
    pub shell_id: String,
    pub shell_name: String,
    pub setup_workspace_cleanup: Option<SetupWorkspaceCleanup>,
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
        Self::attach_with_client(client, shell, rows, cols, pixel_width, pixel_height)
    }

    pub fn restore(
        shell: ShellChoice,
        rows: u16,
        cols: u16,
        pixel_width: u16,
        pixel_height: u16,
    ) -> Result<Self, String> {
        if !matches!(shell.status, ShellStatus::Running) || shell.run_id.is_none() {
            return Err("Saved Shell is stopped; start it explicitly".into());
        }
        let client = client::connect_if_running()
            .map_err(|e| e.to_string())?
            .ok_or("Boomux is not running")?;
        Self::attach_with_policy(client, shell, rows, cols, pixel_width, pixel_height, false)
    }

    fn attach_with_client(
        client: Client,
        shell: ShellChoice,
        rows: u16,
        cols: u16,
        pixel_width: u16,
        pixel_height: u16,
    ) -> Result<Self, String> {
        Self::attach_with_policy(client, shell, rows, cols, pixel_width, pixel_height, true)
    }

    #[allow(clippy::too_many_arguments)]
    fn attach_with_policy(
        client: Client,
        shell: ShellChoice,
        rows: u16,
        cols: u16,
        pixel_width: u16,
        pixel_height: u16,
        takeover: bool,
    ) -> Result<Self, String> {
        let profile = terminal_profile(rows, cols, pixel_width, pixel_height);
        let attachment = attach_shell(&client, &shell, profile.clone(), takeover)?;
        let shared = Arc::new(SharedTerminal::new(profile));
        let stream = attachment.stream;
        shared.install_writer(&stream)?;
        start_emulator(&shared, rows, cols, pixel_width, pixel_height)?;
        shared.process(attachment.reconstruction);

        // An exact-running attach already validated this run on the owner.
        // Only a newly started/restarted Shell needs its new run fetched.
        let expected_run_id =
            if matches!(shell.status, ShellStatus::Running) && shell.run_id.is_some() {
                shell.run_id.clone()
            } else {
                crate::remote::shell(&client, &shell.id)
                    .ok()
                    .and_then(|snapshot| snapshot.run.map(|run| run.id))
            };
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
            setup_workspace_cleanup: None,
            shared,
            // Attachment already established this geometry. Avoid an unchanged
            // first-render resize canceling the reader's temporary redraw size.
            last_size: Mutex::new((rows, cols)),
        })
    }

    pub fn revision(&self) -> u64 {
        self.shared.revision.load(Ordering::Acquire)
    }

    pub fn set_theme(&self, theme: TerminalTheme) -> Result<(), String> {
        self.shared.set_theme(theme)
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
        if let Err(error) = self
            .shared
            .request_resize((rows, cols, pixel_width, pixel_height))
        {
            self.shared.set_status(error);
        }
        true
    }

    pub fn focus(&self) {
        if let Err(error) = self.shared.request_focus() {
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
        self.shared.cancel_emulator();
    }
}

pub fn discover_overview() -> Result<BoomuxOverview, String> {
    discover_overview_and_nodes().0
}

/// Desktop refresh policy, separate from read-only discovery. Only the owner
/// can confirm emptiness; the guarded close rejects concurrent Shell creation.
pub fn refresh_overview_and_nodes() -> (
    Result<BoomuxOverview, String>,
    Result<Vec<crate::nodes::NodeView>, String>,
) {
    let (mut overview, nodes) = discover_overview_and_nodes();
    if let Ok(current) = &mut overview {
        let candidates = current
            .workspaces
            .iter()
            .filter(|workspace| {
                workspace.shells.is_empty()
                    && crate::remote::identity(&workspace.id).is_none_or(|id| {
                        nodes.as_ref().is_ok_and(|nodes| {
                            nodes
                                .iter()
                                .any(|node| node.id == id.node_id && node.connected())
                        })
                    })
            })
            .take(8)
            .map(|workspace| workspace.id.clone())
            .collect::<Vec<_>>();
        if !candidates.is_empty() {
            let cleanup = (|| {
                let client = client::connect_if_running()
                    .map_err(|error| error.to_string())?
                    .ok_or("Boomux is not running")?;
                for id in candidates {
                    if close_empty_workspace(&client, &id).map_err(|error| error.to_string())? {
                        current.workspaces.retain(|workspace| workspace.id != id);
                    }
                }
                Ok::<_, String>(())
            })();
            if let Err(error) = cleanup {
                overview = Err(format!("Could not remove empty Workspace: {error}"));
            }
        }
    }
    (overview, nodes)
}

fn close_empty_workspace(client: &Client, id: &str) -> Result<bool, client::ClientError> {
    use boomux::protocol::{RoutedOperation, RoutedOperationResult};
    let result = (|| {
        let workspace = crate::remote::workspace(client, id)?;
        if !workspace.shells.is_empty() {
            return Ok(false);
        }
        if let Some(owner) = crate::remote::identity(id) {
            match client.route_node_operation(
                owner.node_id,
                RoutedOperation::CloseWorkspace {
                    workspace_id: owner.inner_id,
                    expected_revision: workspace.revision,
                },
            )? {
                RoutedOperationResult::Ok => Ok(true),
                _ => Ok(false),
            }
        } else {
            match client.request(Request::GuardedCloseWorkspace {
                workspace_id: workspace.id,
                expected_revision: workspace.revision,
            })? {
                Response::Ok => Ok(true),
                _ => Ok(false),
            }
        }
    })();
    match result {
        Err(client::ClientError::Remote(error)) if error.code == Some(ErrorCode::NotFound) => {
            Ok(true)
        }
        Err(client::ClientError::Remote(error)) if error.code == Some(ErrorCode::RevisionAhead) => {
            Ok(false)
        }
        result => result,
    }
}

fn discover_local_overview() -> Result<BoomuxOverview, String> {
    let Some(client) = client::connect_if_running()
        .map_err(|error| format!("could not connect to Boomux: {error}"))?
    else {
        return Err("Boomux is not running".into());
    };
    let snapshot = client
        .snapshot()
        .map_err(|error| format!("could not read Boomux workspaces: {error}"))?;
    Ok(overview_from_snapshot(snapshot))
}

pub fn discover_overview_and_nodes() -> (
    Result<BoomuxOverview, String>,
    Result<Vec<crate::nodes::NodeView>, String>,
) {
    let combined = client::connect_if_running()
        .map_err(|error| format!("could not connect to Boomux: {error}"))
        .and_then(|client| client.ok_or_else(|| "Boomux is not running".into()))
        .and_then(|client| {
            client
                .combined_node_snapshot(None)
                .map_err(|error| format!("could not read Boomux Nodes: {error}"))
        });
    match combined {
        Ok(combined) => {
            let nodes = crate::nodes::project(&combined);
            let mut overview = combined
                .nodes
                .iter()
                .find(|node| node.local)
                .and_then(|node| node.local_snapshot.clone())
                .map(overview_from_snapshot)
                .ok_or_else(|| "Boomux omitted the local Node snapshot".into());
            if let Ok(overview) = &mut overview {
                append_remote_workspaces(overview, &combined);
            }
            (overview, Ok(nodes))
        }
        // Federation availability must not stop local discovery.
        Err(error) => (discover_local_overview(), Err(error)),
    }
}

fn append_remote_workspaces(
    overview: &mut BoomuxOverview,
    combined: &boomux::protocol::CombinedNodeSnapshot,
) {
    for node in combined.nodes.iter().filter(|node| !node.local) {
        let Some(projection) = &node.remote_projection else {
            continue;
        };
        let shell_index = projection
            .shells
            .iter()
            .map(|s| (s.id.as_str(), s))
            .collect::<HashMap<_, _>>();
        let workspace_index = projection
            .workspaces
            .iter()
            .map(|w| (w.id.as_str(), w))
            .collect::<HashMap<_, _>>();
        let mut shell_groups = HashMap::<&str, Vec<_>>::new();
        for shell in &projection.shells {
            shell_groups
                .entry(&shell.workspace_id)
                .or_default()
                .push(shell);
        }
        let mut agent_counts = HashMap::<&str, usize>::new();
        for agent in &projection.agents {
            let current = shell_index
                .get(agent.shell_id.as_str())
                .is_some_and(|shell| shell.run_id.as_ref() == Some(&agent.run_id));
            if agent_is_visible(agent.state, agent.attention.is_some(), current) {
                *agent_counts.entry(&agent.workspace_id).or_default() += 1;
            }
        }
        for workspace in &projection.workspaces {
            let shells = shell_groups
                .remove(workspace.id.as_str())
                .unwrap_or_default()
                .into_iter()
                .map(|shell| ShellChoice {
                    id: crate::remote::key(&node.node_id, &shell.id),
                    workspace_id: crate::remote::key(&node.node_id, &workspace.id),
                    name: shell.name.clone(),
                    cwd: PathBuf::new(),
                    status: shell.status.clone(),
                    run_id: shell.run_id.clone(),
                    desktop_setup: false,
                })
                .collect();
            overview.workspaces.push(WorkspaceChoice {
                id: crate::remote::key(&node.node_id, &workspace.id),
                name: workspace.name.clone(),
                shells,
                agent_count: agent_counts
                    .get(workspace.id.as_str())
                    .copied()
                    .unwrap_or_default(),
            });
        }
        for agent in &projection.agents {
            let current = shell_index
                .get(agent.shell_id.as_str())
                .is_some_and(|s| s.run_id.as_ref() == Some(&agent.run_id));
            if !agent_is_visible(agent.state, agent.attention.is_some(), current) {
                continue;
            }
            overview.agents.push(AgentChoice {
                id: crate::remote::key(&node.node_id, &agent.id),
                run_id: agent.run_id.clone(),
                shell_id: crate::remote::key(&node.node_id, &agent.shell_id),
                shell_name: shell_index
                    .get(agent.shell_id.as_str())
                    .map_or_else(|| agent.name.clone(), |s| s.name.clone()),
                display_name: agent.name.clone(),
                workspace: workspace_index
                    .get(agent.workspace_id.as_str())
                    .map_or_else(|| node.alias.clone(), |w| w.name.clone()),
                integration: agent.integration.clone(),
                state: agent.state,
                updated_at_ms: agent.observed_at_ms,
                needs_attention: agent
                    .attention
                    .as_ref()
                    .is_some_and(|a| a.reason == AgentAttentionReason::Blocked),
                completed_attention: agent
                    .attention
                    .as_ref()
                    .is_some_and(|a| a.reason == AgentAttentionReason::Completed),
                attention_revision: agent.attention.as_ref().map(|a| a.observation_revision),
            });
        }
    }
}

fn overview_from_snapshot(snapshot: boomux::protocol::Snapshot) -> BoomuxOverview {
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
                run_id: agent.run_id.clone(),
                shell_name: workspace
                    .shells
                    .iter()
                    .find(|shell| shell.id == agent.shell_id)
                    .map(|shell| shell.name.clone())
                    .unwrap_or_else(|| agent.name.clone()),
                display_name: String::new(),
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
    distinguish_agent_rows(&mut agents);
    agents.sort_by_key(|agent| std::cmp::Reverse(agent.updated_at_ms));
    BoomuxOverview {
        workspaces,
        agents,
        focused_shell_id,
    }
}

// Labels are presentation only; actions retain the exact Agent and Shell IDs.
// Work is bounded by the visible overview and runs outside GPUI rendering.
fn distinguish_agent_rows(agents: &mut [AgentChoice]) {
    let mut order: Vec<usize> = (0..agents.len()).collect();
    order.sort_unstable_by(|&a, &b| {
        (&agents[a].shell_id, &agents[a].id).cmp(&(&agents[b].shell_id, &agents[b].id))
    });
    for (position, &index) in order.iter().enumerate() {
        let agent = &agents[index];
        let mut shared_shell = false;
        let mut prefix_len = 8;
        for neighbor in [position.checked_sub(1), position.checked_add(1)]
            .into_iter()
            .flatten()
            .filter_map(|position| order.get(position))
        {
            let other = &agents[*neighbor];
            if agent.shell_id == other.shell_id {
                shared_shell = true;
                prefix_len = prefix_len.max(
                    agent
                        .id
                        .chars()
                        .zip(other.id.chars())
                        .take_while(|(a, b)| a == b)
                        .count()
                        + 1,
                );
            }
        }
        agents[index].display_name = if shared_shell {
            format!(
                "{} · {}",
                agent.shell_name,
                agent.id.chars().take(prefix_len).collect::<String>()
            )
        } else {
            agent.shell_name.clone()
        };
    }
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
    if let Some(id) = crate::remote::identity(agent_id) {
        return client
            .route_node_operation(
                id.node_id,
                boomux::protocol::RoutedOperation::AcknowledgeAgentAttention {
                    agent_id: id.inner_id,
                    observation_revision,
                },
            )
            .map(|_| ())
            .map_err(|e| e.to_string());
    }
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
        desktop_setup: match shell.command.get(1).map(String::as_str) {
            Some("__desktop-setup" | "__guided-node-add") => shell.command.len() == 2,
            Some(
                "__guided-node-upgrade"
                | "__guided-node-reauthenticate"
                | "__guided-node-uninstall",
            ) => shell.command.len() == 3,
            _ => false,
        },
    }
}

/// Create a pending shell next to an existing shell. Boomux remains the owner
/// of the PTY; the caller can immediately attach the returned choice.
pub fn create_shell(
    anchor: &ShellChoice,
    size: (u16, u16, u16, u16),
) -> Result<ShellChoice, String> {
    if crate::remote::identity(&anchor.workspace_id).is_some() {
        return create_shell_in_workspace(&anchor.workspace_id, size);
    }
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
        .create_started_shell(
            &workspace.id,
            ShellSpec::login(
                name,
                workspace.default_cwd.unwrap_or_else(|| anchor.cwd.clone()),
            ),
            terminal_profile(size.0, size.1, size.2, size.3),
        )
        .map_err(|error| format!("could not create Boomux shell: {error}"))?;
    Ok(shell_choice(shell))
}

pub fn create_shell_in_workspace(
    workspace_id: &str,
    size: (u16, u16, u16, u16),
) -> Result<ShellChoice, String> {
    let Some(client) = client::connect_if_running()
        .map_err(|error| format!("could not connect to Boomux: {error}"))?
    else {
        return Err("Boomux is not running".into());
    };
    if crate::remote::identity(workspace_id).is_some() {
        return crate::remote::create_shell(&client, workspace_id).map(shell_choice);
    }
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
        .create_started_shell(
            &workspace.id,
            ShellSpec::login(name, cwd),
            terminal_profile(size.0, size.1, size.2, size.3),
        )
        .map(shell_choice)
        .map_err(|error| format!("could not create Boomux shell: {error}"))
}

/// Explicit local terminal flows using the matching Boomux executable.
#[derive(Clone, Debug)]
pub enum WorkspaceLaunch {
    Shell,
    Project {
        name: String,
        path: std::path::PathBuf,
    },
    Setup,
    ConfigEdit,
    AddNode,
    RemoteWorkspace {
        node_id: String,
        name: String,
    },
    UpgradeNode(String),
    UninstallNode(String),
    ReauthenticateNode(String),
}

impl WorkspaceLaunch {
    fn temporary_setup(&self) -> bool {
        matches!(
            self,
            Self::Setup
                | Self::AddNode
                | Self::UpgradeNode(_)
                | Self::ReauthenticateNode(_)
                | Self::UninstallNode(_)
        )
    }
    fn working_directory(&self) -> Result<std::path::PathBuf, String> {
        if let Self::Project { path, .. } = self {
            let path = path
                .canonicalize()
                .map_err(|error| format!("Project folder is unavailable: {error}"))?;
            if !path.is_dir() {
                return Err("Project path is no longer a directory".into());
            }
            Ok(path)
        } else {
            std::env::current_dir().map_err(|error| {
                format!("could not determine the new workspace directory: {error}")
            })
        }
    }

    fn command(&self) -> Option<(&'static str, Vec<String>)> {
        match self {
            Self::Shell | Self::Project { .. } | Self::RemoteWorkspace { .. } => None,
            Self::Setup => Some(("Set up agents", vec!["__desktop-setup".into()])),
            Self::ConfigEdit => Some(("Edit Boomux config", vec!["config".into(), "edit".into()])),
            Self::AddNode => Some(("Connect remote machine", vec!["__guided-node-add".into()])),
            Self::UpgradeNode(id) => Some((
                "Update remote Boomux",
                vec!["__guided-node-upgrade".into(), id.clone()],
            )),
            Self::UninstallNode(id) => Some((
                "Remove remote machine",
                vec!["__guided-node-uninstall".into(), id.clone()],
            )),
            Self::ReauthenticateNode(id) => Some((
                "Sign in to remote machine",
                vec!["__guided-node-reauthenticate".into(), id.clone()],
            )),
        }
    }
}

pub(crate) fn project_workspace_name<'a>(
    name: &str,
    existing: impl Iterator<Item = &'a str>,
) -> String {
    let existing: std::collections::HashSet<_> = existing.collect();
    if !existing.contains(name) {
        return name.to_owned();
    }
    for suffix in 2..=existing.len() + 2 {
        let candidate = format!("{name}-{suffix}");
        if !existing.contains(candidate.as_str()) {
            return candidate;
        }
    }
    unreachable!()
}

/// Create a local Workspace and its first pending Shell; Boomux owns both.
pub fn create_workspace_with_shell(
    launch: WorkspaceLaunch,
) -> Result<(ShellChoice, Option<SetupWorkspaceCleanup>), String> {
    let Some(client) = client::connect_if_running()
        .map_err(|error| format!("could not connect to Boomux: {error}"))?
    else {
        return Err("Boomux is not running".into());
    };
    let setup_node = if launch.temporary_setup() {
        Some(client.node_identity().map_err(|error| error.to_string())?)
    } else {
        None
    };
    if let WorkspaceLaunch::RemoteWorkspace { node_id, name } = &launch {
        return crate::remote::create_workspace(&client, node_id, name)
            .map(|shell| (shell_choice(shell), None));
    }
    let snapshot = client
        .snapshot()
        .map_err(|error| format!("could not read Boomux workspaces: {error}"))?;
    let workspace_name = if let WorkspaceLaunch::Project { name, .. } = &launch {
        project_workspace_name(
            name,
            snapshot
                .workspaces
                .iter()
                .map(|workspace| workspace.name.as_str()),
        )
    } else {
        generated_names::random_excluding(
            snapshot
                .workspaces
                .iter()
                .map(|workspace| workspace.name.as_str()),
        )
        .ok_or_else(|| "Boomux workspace names are exhausted".to_string())?
    };
    let shell_name = generated_names::random_excluding(std::iter::empty())
        .ok_or_else(|| "Boomux shell names are exhausted".to_string())?;
    let cwd = launch.working_directory()?;
    let mut spec = ShellSpec::login(shell_name, cwd.clone());
    if let Some((label, arguments)) = launch.command() {
        let executable = std::env::current_exe().map_err(|error| error.to_string())?;
        let bundled = executable
            .parent()
            .and_then(|path| path.parent())
            .map(|path| path.join("bin/boomux"))
            .filter(|path| path.is_file());
        let cli = bundled
            .or_else(|| {
                executable
                    .parent()
                    .map(|path| path.join("boomux"))
                    .filter(|path| path.is_file())
            })
            .ok_or_else(|| "Cannot find the matching Boomux executable for setup".to_string())?;
        spec.command = vec![
            cli.to_str()
                .ok_or("The setup executable path must be valid UTF-8")?
                .to_owned(),
        ];
        spec.command.extend(arguments);
        spec.name = label.into();
    }
    let workspace = client
        .create_workspace_with_default_cwd(workspace_name, Some(cwd.clone()), vec![spec])
        .map_err(|error| format!("could not create Boomux workspace: {error}"))?;
    let cleanup = setup_node
        .and_then(|node_id| SetupWorkspaceCleanup::from_creation(&launch, node_id, &workspace));
    workspace
        .shells
        .into_iter()
        .next()
        .map(|shell| (shell_choice(shell), cleanup))
        .ok_or_else(|| "Boomux created the workspace without its initial shell".to_string())
}

/// Ephemeral proof from this setup launch's successful CreateWorkspace response.
/// Discovery and reattachment never reconstruct cleanup ownership from a name or command.
pub struct SetupWorkspaceCleanup {
    node_id: String,
    workspace_id: String,
    empty_revision: u64,
}

impl SetupWorkspaceCleanup {
    fn from_creation(
        launch: &WorkspaceLaunch,
        node_id: String,
        workspace: &WorkspaceSnapshot,
    ) -> Option<Self> {
        if !launch.temporary_setup()
            || workspace.shells.len() != 1
            || !workspace.launchers.is_empty()
            || !workspace.agents.is_empty()
        {
            return None;
        }
        if launch.command()?.1 != workspace.shells[0].command.get(1..)? {
            return None;
        }
        Some(Self {
            node_id,
            workspace_id: workspace.id.clone(),
            // Removing the sole setup Shell is the only allowed intervening mutation.
            empty_revision: workspace.revision.checked_add(1)?,
        })
    }

    fn close_request(&self, node_id: &str, workspace: &WorkspaceSnapshot) -> Option<Request> {
        (node_id == self.node_id
            && workspace.id == self.workspace_id
            && workspace.revision == self.empty_revision
            && workspace.shells.is_empty()
            && workspace.launchers.is_empty()
            && workspace.agents.is_empty())
        .then(|| Request::GuardedCloseWorkspace {
            workspace_id: self.workspace_id.clone(),
            expected_revision: self.empty_revision,
        })
    }

    fn cleanup(&self, client: &Client) -> Result<(), String> {
        let node_id = client.node_identity().map_err(|error| error.to_string())?;
        if node_id != self.node_id {
            return Ok(());
        }
        // RouteNodeOperation addresses registered remote Nodes, not this local owner.
        let workspace = match client.get_workspace(&self.workspace_id) {
            Ok(workspace) => workspace,
            Err(client::ClientError::Remote(error)) if error.code == Some(ErrorCode::NotFound) => {
                return Ok(());
            }
            Err(error) => return Err(error.to_string()),
        };
        let Some(request) = self.close_request(&node_id, &workspace) else {
            return Ok(());
        };
        // No unguarded fallback or retry against a newer revision after a race.
        match client.request(request).map_err(|error| error.to_string())? {
            Response::Ok => Ok(()),
            response => Err(format!(
                "unexpected Workspace cleanup response: {response:?}"
            )),
        }
    }
}

pub fn cleanup_setup_workspace(cleanup: SetupWorkspaceCleanup) -> Result<(), String> {
    let client = client::connect_if_running()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "Boomux is not running".to_string())?;
    cleanup.cleanup(&client)
}

pub fn rename_workspace(workspace_id: &str, name: &str) -> Result<(), String> {
    let Some(client) = client::connect_if_running()
        .map_err(|error| format!("could not connect to Boomux: {error}"))?
    else {
        return Err("Boomux is not running".into());
    };
    if crate::remote::identity(workspace_id).is_some() {
        return crate::remote::rename(&client, workspace_id, name, true);
    }
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
    if crate::remote::identity(shell_id).is_some() {
        return crate::remote::rename(&client, shell_id, name, false);
    }
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
    if crate::remote::identity(workspace_id).is_some() {
        return crate::remote::close(&client, workspace_id, true);
    }
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
    if crate::remote::identity(shell_id).is_some() {
        return crate::remote::close(&client, shell_id, false);
    }
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
    if let Some(identity) = crate::remote::identity(&shell.id) {
        return client
            .attach_node(
                identity,
                takeover,
                matches!(shell.status, ShellStatus::Exited { .. }),
                shell.run_id.clone(),
                profile,
            )
            .map_err(|error| format!("could not attach {}: {error}", shell.name));
    }
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
            EmulatorCommand::FocusLatest => {
                unreachable!("focus notifications are resolved by the emulator worker")
            }
            EmulatorCommand::ResizeLatest => {
                unreachable!("UI resize requests are resolved by the emulator worker")
            }
            EmulatorCommand::MouseWheel { .. } => {
                unreachable!("mouse events are resolved by the emulator worker")
            }
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
    if shared.cancelled.load(Ordering::Acquire) {
        return Ok(false);
    }
    shared.flush_pending_resize(core)?;
    shared.flush_pending_focus()?;
    match command {
        EmulatorCommand::FocusLatest | EmulatorCommand::ResizeLatest => Ok(true),
        EmulatorCommand::Output(bytes) => {
            // A reconstruction can contain a large transcript. Yield to pane
            // cancellation between chunks without changing byte ordering.
            for chunk in bytes.chunks(16 * 1024) {
                if shared.cancelled.load(Ordering::Acquire) {
                    return Ok(false);
                }
                shared.flush_pending_resize(core)?;
                shared.flush_pending_focus()?;
                core.terminal.vt_write(chunk);
            }
            Ok(true)
        }
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

            run_emulator(&mut core, &worker_shared, receiver);
            worker_shared.updates.close();
        })
        .map_err(|error| format!("could not start Ghostty terminal worker: {error}"))?;

    ready_receiver
        .recv()
        .map_err(|_| "Ghostty terminal worker stopped during startup".to_string())??;
    shared.install_emulator(sender);
    Ok(())
}

fn run_emulator(
    core: &mut EmulatorCore,
    worker_shared: &SharedTerminal,
    receiver: mpsc::Receiver<EmulatorCommand>,
) {
    while let Ok(command) = receiver.recv() {
        match apply_emulator_command(core, worker_shared, command) {
            Ok(true) => {}
            Ok(false) => break,
            Err(error) => {
                worker_shared.close(error);
                return;
            }
        }

        let mut stopped = false;
        while let Ok(command) = receiver.try_recv() {
            match apply_emulator_command(core, worker_shared, command) {
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
            break;
        }
        // A full queue could not accept the wake-up marker, but it did
        // keep the latest requested row. Apply that row once after the
        // lossless command queue has been drained.
        if worker_shared
            .pending_scroll_wakeup
            .swap(false, Ordering::AcqRel)
        {
            let row = worker_shared.pending_scroll_row.load(Ordering::Acquire) as usize;
            if let Err(error) = core.apply(EmulatorCommand::Scroll(ScrollViewport::Row(row))) {
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
        if let Err(error) = publish_screen(core, worker_shared) {
            worker_shared.close(error);
            return;
        }
    }
    // Transport closure may end synchronized output. Preserve the final screen
    // unless the pane itself was discarded and cancelled its replay.
    if !worker_shared.cancelled.load(Ordering::Acquire)
        && let Err(error) = publish_screen(core, worker_shared)
    {
        worker_shared.close(error);
    }
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
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("boomux-desktop-terminal".into())
        .spawn(move || {
            let mut refresh_attachment = true;
            loop {
                if refresh_attachment {
                    if let Err(error) = resynchronize_terminal_size(&shared, thread::sleep) {
                        shared.close(format!("could not restore terminal size: {error}"));
                        return;
                    }
                    refresh_attachment = false;
                }
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
                                if let Err(error) = shared.install_writer(&stream) {
                                    shared.close(error);
                                    return;
                                }
                                shared.process(attachment.reconstruction);
                                refresh_attachment = true;
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
        .expect("spawn Boomux terminal reader")
}

fn resynchronize_terminal_size(
    shared: &SharedTerminal,
    settle: impl FnOnce(Duration),
) -> Result<(), String> {
    // An unchanged TIOCSWINSZ need not notify the foreground application.
    // Change the width once so TUIs can reflow their transcript after attaching
    // to a running Shell. Keep the delay on the existing socket reader thread.
    {
        let profile = shared.profile.lock().unwrap();
        shared.send(AttachFrame::Resize {
            rows: profile.rows,
            cols: if profile.cols > 1 {
                profile.cols - 1
            } else {
                2
            },
            pixel_width: profile.pixel_width,
            pixel_height: profile.pixel_height,
        })?;
    }
    settle(RESIZE_SETTLE);
    // A pane may resize while the application redraws. Restore the latest
    // geometry, serialized with profile updates and other attachment writes.
    let profile = shared.profile.lock().unwrap();
    shared.send(AttachFrame::Resize {
        rows: profile.rows,
        cols: profile.cols,
        pixel_width: profile.pixel_width,
        pixel_height: profile.pixel_height,
    })
}

fn reconnect(
    client: &Client,
    shell_id: &str,
    expected_run_id: Option<&str>,
    profile: &TerminalProfile,
) -> Result<client::Attachment, String> {
    let mut last_error = None;
    for _ in 0..RECONNECT_ATTEMPTS {
        let result = if let Some(identity) = crate::remote::identity(shell_id) {
            client.attach_node(
                identity,
                false,
                false,
                expected_run_id.map(str::to_owned),
                profile.clone(),
            )
        } else if let Some(run_id) = expected_run_id {
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
    #[test]
    #[ignore = "requires the matching CLI in BOOMUX_TEST_CLI; runs only an isolated fixture daemon"]
    fn setup_workspace_real_lifecycle_cleans_only_unused_creation() {
        use boomux::protocol::{Request, ShellSpec};
        use std::os::unix::fs::DirBuilderExt;
        use std::process::{Command, Stdio};

        struct Fixture {
            root: std::path::PathBuf,
            child: std::process::Child,
            client: boomux::client::Client,
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = self.client.shutdown();
                let _ = self.child.kill();
                let _ = self.child.wait();
                let _ = std::fs::remove_dir_all(&self.root);
            }
        }
        let executable = std::env::var_os("BOOMUX_TEST_CLI")
            .expect("set BOOMUX_TEST_CLI to this worktree's built CLI");
        let root =
            std::env::temp_dir().join(format!("desktop-setup-lifecycle-{}", fastrand::u64(..)));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .unwrap();
        let child = Command::new(executable)
            .args(["daemon", "run"])
            .env_clear()
            .env("HOME", &root)
            .env("PATH", "/usr/bin:/bin")
            .env("SHELL", "/bin/sh")
            .env("XDG_RUNTIME_DIR", &root)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", root.join("state"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let fixture = Fixture {
            client: boomux::client::Client::from_socket_path(root.join("boomux/daemon.sock")),
            root,
            child,
        };
        let wait = |condition: &mut dyn FnMut() -> bool| {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !condition() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "isolated lifecycle timed out"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        wait(&mut || fixture.client.ping().is_ok());
        for (reuse, race) in [(false, false), (true, false), (false, true)] {
            let mut spec = ShellSpec::login("fixture-setup", fixture.root.clone());
            // A harmless waiting process exercises the same PTY/start/close path, without setup/install.
            spec.command = vec![
                "/usr/bin/env".into(),
                "-i".into(),
                format!("HOME={}", fixture.root.display()),
                format!("XDG_RUNTIME_DIR={}", fixture.root.display()),
                format!("XDG_CONFIG_HOME={}", fixture.root.join("config").display()),
                format!("XDG_STATE_HOME={}", fixture.root.join("state").display()),
                "PATH=/usr/bin:/bin".into(),
                "/bin/sh".into(),
                "-c".into(),
                "printf fixture-ready; read answer".into(),
            ];
            let created = fixture
                .client
                .create_workspace(format!("fixture-{reuse}-{race}"), vec![spec])
                .unwrap();
            let receipt = super::SetupWorkspaceCleanup::from_creation(
                &super::WorkspaceLaunch::Setup,
                fixture.client.node_identity().unwrap(),
                &created,
            )
            .unwrap();
            let mut shell = super::shell_choice(created.shells[0].clone());
            shell.desktop_setup = true;
            let mut session = super::TerminalSession::attach_with_client(
                fixture.client.clone(),
                shell.clone(),
                24,
                80,
                800,
                480,
            )
            .unwrap();
            session.setup_workspace_cleanup = Some(receipt);
            wait(&mut || {
                fixture
                    .client
                    .read_shell(&shell.id, 1024)
                    .is_ok_and(|bytes| bytes.windows(13).any(|bytes| bytes == b"fixture-ready"))
            });
            let started = fixture.client.get_workspace(&created.id).unwrap();
            assert_eq!(
                started.revision, created.revision,
                "PTY startup must not be mistaken for user reuse"
            );
            if reuse {
                let user_shell = fixture
                    .client
                    .create_shell(
                        &created.id,
                        ShellSpec::login("user-work", fixture.root.clone()),
                    )
                    .unwrap();
                fixture.client.close_shell(&user_shell.id).unwrap();
            }
            let running = fixture.client.get_shell(&shell.id).unwrap();
            fixture
                .client
                .request(Request::GuardedCloseShell {
                    shell_id: shell.id.clone(),
                    expected_revision: running.revision,
                })
                .unwrap();
            wait(&mut || session.update_events().is_closed());
            let overview = super::overview_from_snapshot(fixture.client.snapshot().unwrap());
            assert!(crate::setup_pane_was_removed(&shell, true, &overview));
            let empty = fixture.client.get_workspace(&created.id).unwrap();
            assert!(empty.shells.is_empty());
            eprintln!(
                "setup lifecycle reuse={reuse}: created={}, started={}, removed={}",
                created.revision, started.revision, empty.revision
            );
            assert_eq!(empty.revision, created.revision + if reuse { 3 } else { 1 });
            let cleanup = session.setup_workspace_cleanup.take().unwrap();
            if race {
                let stale = cleanup
                    .close_request(&fixture.client.node_identity().unwrap(), &empty)
                    .unwrap();
                let user_shell = fixture
                    .client
                    .create_shell(
                        &created.id,
                        ShellSpec::login("concurrent-user-work", fixture.root.clone()),
                    )
                    .unwrap();
                assert!(
                    matches!(fixture.client.request(stale), Err(boomux::client::ClientError::Remote(error)) if error.code == Some(boomux::protocol::ErrorCode::RevisionAhead))
                );
                assert!(fixture.client.get_shell(&user_shell.id).is_ok());
            }
            cleanup.cleanup(&fixture.client).unwrap();
            match fixture.client.get_workspace(&created.id) {
                Ok(workspace) if reuse || race => assert_eq!(workspace.id, created.id),
                Err(boomux::client::ClientError::Remote(error)) if !reuse && !race => {
                    assert_eq!(error.code, Some(boomux::protocol::ErrorCode::NotFound));
                }
                result => {
                    panic!("unexpected cleanup result (reuse={reuse}, race={race}): {result:?}")
                }
            }
        }
    }

    fn setup_workspace_creation() -> boomux::protocol::WorkspaceSnapshot {
        serde_json::from_value(serde_json::json!({
            "id": "created-workspace", "name": "same-name-as-another-workspace", "revision": 1,
            "shells": [{"id": "setup-shell", "workspace_id": "created-workspace",
                "name": "Set up agents", "cwd": "/tmp", "status": "pending",
                "command": ["/build/boomux", "__desktop-setup"]}]
        }))
        .unwrap()
    }

    #[test]
    fn remote_setup_cleanup_owns_only_the_created_temporary_workspace() {
        for launch in [
            super::WorkspaceLaunch::AddNode,
            super::WorkspaceLaunch::UpgradeNode("remote".into()),
            super::WorkspaceLaunch::UninstallNode("remote".into()),
            super::WorkspaceLaunch::ReauthenticateNode("remote".into()),
        ] {
            let mut created = setup_workspace_creation();
            created.shells[0].command = vec!["/build/boomux".into()];
            created.shells[0]
                .command
                .extend(launch.command().unwrap().1);
            assert!(super::shell_choice(created.shells[0].clone()).desktop_setup);
            let cleanup = super::SetupWorkspaceCleanup::from_creation(
                &launch,
                "local-owner".into(),
                &created,
            )
            .unwrap();
            assert!(cleanup.close_request("local-owner", &created).is_none());
            created.shells.clear();
            created.revision += 1;
            assert!(cleanup.close_request("local-owner", &created).is_some());
            assert!(cleanup.close_request("remote", &created).is_none());
            created.revision += 1;
            assert!(cleanup.close_request("local-owner", &created).is_none());
        }
    }

    #[test]
    fn remote_projection_keeps_owner_identity_and_cached_shells() {
        let nodes = ["first", "second"].map(|owner| serde_json::json!({
            "node_id": owner, "alias": "same-machine-label", "local": false,
            "health": "unreachable", "current": false, "stale": true, "observed_at_ms": 1,
            "remote_projection": {"node_id": owner,
                "workspaces": [{"id": "same-workspace", "name": "work", "item_count": 1, "attention_count": 0}],
                "shells": [{"id": "same-shell", "workspace_id": "same-workspace", "name": "shell", "status": "running", "run_id": "run"}],
                "agents": [], "launchers": []}
        }));
        let combined = serde_json::from_value(serde_json::json!({"nodes": nodes})).unwrap();
        let mut overview = super::BoomuxOverview::default();
        super::append_remote_workspaces(&mut overview, &combined);
        assert_eq!(overview.workspaces.len(), 2);
        let first = &overview.workspaces[0];
        let second = &overview.workspaces[1];
        assert_ne!(first.id, second.id);
        assert_ne!(first.shells[0].id, second.shells[0].id);
        assert_eq!(first.shells[0].workspace_id, first.id);
        assert_eq!(
            first.shells[0].status,
            boomux::protocol::ShellStatus::Running
        );
        assert!(first.shells[0].cwd.as_os_str().is_empty());
    }

    #[test]
    fn layout_restore_requests_exact_run_without_restart_or_takeover() {
        use boomux::protocol::{self, Envelope, Request, Response, ShellStatus};
        use std::os::unix::net::UnixListener;
        let directory = std::env::temp_dir().join(format!("la-{:016x}", fastrand::u64(..)));
        std::fs::create_dir(&directory).unwrap();
        let socket = directory.join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let envelope: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            let Request::Attach {
                shell_id,
                expected_run_id,
                takeover,
                restart_exited,
                ..
            } = envelope.message
            else {
                panic!("expected exact attachment")
            };
            assert_eq!(shell_id, "saved-shell");
            assert_eq!(expected_run_id.as_deref(), Some("saved-run"));
            assert!(!takeover && !restart_exited);
            protocol::write_message(
                &mut stream,
                &Envelope::with_version(
                    envelope.version,
                    Response::Error {
                        code: None,
                        message: "run exited during restore".into(),
                    },
                ),
            )
            .unwrap();
        });
        let shell = super::ShellChoice {
            id: "saved-shell".into(),
            name: "saved".into(),
            workspace_id: "w".into(),
            cwd: directory.clone(),
            status: ShellStatus::Running,
            run_id: Some("saved-run".into()),
            desktop_setup: false,
        };
        let result = super::TerminalSession::attach_with_policy(
            boomux::client::Client::from_socket_path(socket),
            shell,
            24,
            80,
            800,
            480,
            false,
        );
        assert!(result.is_err());
        server.join().unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn layout_restore_never_starts_pending_or_exited_shells() {
        use boomux::protocol::ShellStatus;
        for status in [ShellStatus::Pending, ShellStatus::Exited { code: Some(0) }] {
            let shell = super::ShellChoice {
                id: "saved".into(),
                name: "saved".into(),
                workspace_id: "workspace".into(),
                cwd: std::path::PathBuf::new(),
                status,
                run_id: None,
                desktop_setup: false,
            };
            let error = super::TerminalSession::restore(shell, 24, 80, 800, 480)
                .err()
                .unwrap();
            assert!(error.contains("start it explicitly"));
        }
    }

    #[test]
    fn remote_attachment_and_reconnect_keep_exact_owner_and_run() {
        use boomux::protocol::{self, Envelope, Request, Response};
        use std::os::unix::net::UnixListener;
        let directory =
            std::env::temp_dir().join(format!("remote-attach-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let socket = directory.join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            for takeover in [true, false] {
                let (mut stream, _) = listener.accept().unwrap();
                let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
                let Request::AttachNode {
                    identity,
                    takeover: actual,
                    restart_exited,
                    expected_run_id,
                    ..
                } = request.message
                else {
                    panic!("must not attach locally")
                };
                assert_eq!(identity.node_id, "owner");
                assert_eq!(identity.inner_id, "shell");
                assert_eq!(actual, takeover);
                assert!(!restart_exited);
                assert_eq!(expected_run_id.as_deref(), Some("run"));
                protocol::write_message(
                    &mut stream,
                    &Envelope::with_version(
                        protocol::PROTOCOL_VERSION,
                        Response::Attached {
                            token: "token".into(),
                            reconstruction: vec![],
                            warning: None,
                            profile: None,
                        },
                    ),
                )
                .unwrap();
            }
        });
        let client = boomux::client::Client::from_socket_path(socket);
        let shell = super::ShellChoice {
            id: crate::remote::key("owner", "shell"),
            workspace_id: crate::remote::key("owner", "workspace"),
            name: "shell".into(),
            cwd: std::path::PathBuf::new(),
            status: boomux::protocol::ShellStatus::Running,
            run_id: Some("run".into()),
            desktop_setup: false,
        };
        let profile = terminal_profile(24, 80, 800, 480);
        super::attach_shell(&client, &shell, profile.clone(), true).unwrap();
        super::reconnect(&client, &shell.id, Some("run"), &profile).unwrap();
        server.join().unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn empty_workspace_cleanup_rechecks_shells_and_rejects_concurrent_changes() {
        use boomux::protocol::{self, Envelope, ErrorCode, Request, Response};
        use std::os::unix::net::UnixListener;

        for (has_shell, race) in [(true, false), (false, false), (false, true)] {
            let directory =
                std::env::temp_dir().join(format!("desktop-empty-cleanup-{}", fastrand::u64(..)));
            std::fs::create_dir(&directory).unwrap();
            let socket = directory.join("daemon.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let mut workspace = setup_workspace_creation();
            if !has_shell {
                workspace.shells.clear();
            }
            workspace.revision = 7;
            let (finished_sender, finished_receiver) = std::sync::mpsc::channel();
            let server = std::thread::spawn(move || {
                let mut exchanges = vec![(
                    Request::GetWorkspace {
                        workspace_id: workspace.id.clone(),
                    },
                    Response::Workspace { workspace },
                )];
                if !has_shell {
                    exchanges.push((
                        Request::GuardedCloseWorkspace {
                            workspace_id: "created-workspace".into(),
                            expected_revision: 7,
                        },
                        if race {
                            Response::Error {
                                code: Some(ErrorCode::RevisionAhead),
                                message: "Shell added after inspection".into(),
                            }
                        } else {
                            Response::Ok
                        },
                    ));
                }
                for (expected, response) in exchanges {
                    let (mut stream, _) = listener.accept().unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
                    assert_eq!(request.message, expected);
                    protocol::write_message(
                        &mut stream,
                        &Envelope::with_version(request.version, response),
                    )
                    .unwrap();
                }
                finished_receiver
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap();
                listener.set_nonblocking(true).unwrap();
                assert!(
                    listener.accept().is_err(),
                    "never retry an empty-workspace close after a race"
                );
            });
            let result = super::close_empty_workspace(
                &boomux::client::Client::from_socket_path(socket),
                "created-workspace",
            )
            .unwrap();
            assert_eq!(result, !has_shell && !race);
            finished_sender.send(()).unwrap();
            server.join().unwrap();
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn setup_workspace_cleanup_requires_creation_identity_and_only_its_shell_removal() {
        let created = setup_workspace_creation();
        let cleanup = super::SetupWorkspaceCleanup::from_creation(
            &super::WorkspaceLaunch::Setup,
            "owner".into(),
            &created,
        )
        .unwrap();
        for launch in [
            super::WorkspaceLaunch::Shell,
            super::WorkspaceLaunch::ConfigEdit,
            super::WorkspaceLaunch::AddNode,
        ] {
            assert!(
                super::SetupWorkspaceCleanup::from_creation(&launch, "owner".into(), &created)
                    .is_none()
            );
        }
        assert!(cleanup.close_request("owner", &created).is_none());
        let mut empty = created.clone();
        empty.shells.clear();
        empty.revision += 1;
        assert_eq!(
            cleanup.close_request("owner", &empty),
            Some(boomux::protocol::Request::GuardedCloseWorkspace {
                workspace_id: created.id.clone(),
                expected_revision: 2,
            })
        );
        assert!(cleanup.close_request("different-node", &empty).is_none());
        empty.id = "existing-workspace-with-the-same-name".into();
        assert!(cleanup.close_request("owner", &empty).is_none());
        empty.id = created.id.clone();
        // Rename/default edits, or adding then removing user work, invalidate ownership.
        for revision in [0, 1, 3, 4, u64::MAX] {
            empty.revision = revision;
            assert!(cleanup.close_request("owner", &empty).is_none());
        }
        // An empty/reused snapshot cannot be a successful setup creation receipt.
        assert!(
            super::SetupWorkspaceCleanup::from_creation(
                &super::WorkspaceLaunch::Setup,
                "owner".into(),
                &empty
            )
            .is_none()
        );
        let mut overflow = created;
        overflow.revision = u64::MAX;
        assert!(
            super::SetupWorkspaceCleanup::from_creation(
                &super::WorkspaceLaunch::Setup,
                "owner".into(),
                &overflow
            )
            .is_none()
        );
    }

    #[test]
    fn setup_workspace_cleanup_preserves_shells_launchers_and_agent_history() {
        let mut workspace = setup_workspace_creation();
        let cleanup = super::SetupWorkspaceCleanup::from_creation(
            &super::WorkspaceLaunch::Setup,
            "owner".into(),
            &workspace,
        )
        .unwrap();
        workspace.revision = 2;
        assert!(cleanup.close_request("owner", &workspace).is_none());
        workspace.shells.clear();
        workspace.launchers.push(
            serde_json::from_value(serde_json::json!({
                "id": "user-launcher", "workspace_id": workspace.id, "name": "user work",
                "command": ["user-command"], "cwd": "/tmp"
            }))
            .unwrap(),
        );
        assert!(cleanup.close_request("owner", &workspace).is_none());
        workspace.launchers.clear();
        workspace.agents.push(serde_json::from_value(serde_json::json!({
            "id": "user-agent", "workspace_id": workspace.id, "shell_id": "old-shell",
            "run_id": "old-run", "name": "history", "integration": "test",
            "started_at_ms": 1, "ended_at_ms": 2,
            "observation": {"revision": 1, "state": "done", "authority": "lifecycle_integration",
                "evidence": "test", "confidence": 100, "observed_at_ms": 2}
        })).unwrap());
        assert!(cleanup.close_request("owner", &workspace).is_none());
    }

    #[test]
    fn setup_workspace_cleanup_sends_guarded_close_once_and_never_retries_a_race() {
        use boomux::protocol::{self, Envelope, ErrorCode, Request, Response};
        use std::os::unix::net::UnixListener;

        for race in [false, true] {
            let directory =
                std::env::temp_dir().join(format!("desktop-setup-cleanup-{}", fastrand::u64(..)));
            std::fs::create_dir(&directory).unwrap();
            let socket = directory.join("daemon.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let mut workspace = setup_workspace_creation();
            let cleanup = super::SetupWorkspaceCleanup::from_creation(
                &super::WorkspaceLaunch::Setup,
                "owner".into(),
                &workspace,
            )
            .unwrap();
            workspace.shells.clear();
            workspace.revision = 2;
            let (finished_sender, finished_receiver) = std::sync::mpsc::channel();
            let server = std::thread::spawn(move || {
                let exchanges = [
                    (
                        Request::GetNodeIdentity,
                        Response::NodeIdentity {
                            node_id: "owner".into(),
                        },
                    ),
                    (
                        Request::GetWorkspace {
                            workspace_id: workspace.id.clone(),
                        },
                        Response::Workspace { workspace },
                    ),
                    (
                        Request::GuardedCloseWorkspace {
                            workspace_id: "created-workspace".into(),
                            expected_revision: 2,
                        },
                        if race {
                            Response::Error {
                                code: Some(ErrorCode::RevisionAhead),
                                message: "user added work after inspection".into(),
                            }
                        } else {
                            Response::Ok
                        },
                    ),
                ];
                for (expected, response) in exchanges {
                    let (mut stream, _) = listener.accept().unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
                    assert_eq!(request.message, expected);
                    protocol::write_message(
                        &mut stream,
                        &Envelope::with_version(request.version, response),
                    )
                    .unwrap();
                }
                finished_receiver
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap();
                listener.set_nonblocking(true).unwrap();
                assert!(
                    listener.accept().is_err(),
                    "cleanup must not retry with an unguarded or updated request"
                );
            });
            let result = cleanup.cleanup(&boomux::client::Client::from_socket_path(socket));
            finished_sender.send(()).unwrap();
            assert_eq!(result.is_err(), race);
            if let Err(error) = result {
                assert!(error.contains("user added work"), "{error}");
            }
            server.join().unwrap();
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn setup_launch_uses_the_dedicated_cleanup_entry_point() {
        assert_eq!(
            super::WorkspaceLaunch::Setup.command().unwrap().1,
            ["__desktop-setup"]
        );
    }

    #[test]
    fn project_launch_uses_the_selected_directory_without_a_command() {
        let path = std::env::temp_dir();
        let launch = super::WorkspaceLaunch::Project {
            name: "project with spaces".into(),
            path: path.clone(),
        };
        assert_eq!(
            launch.working_directory().unwrap(),
            path.canonicalize().unwrap()
        );
        assert!(launch.command().is_none());
        let missing = super::WorkspaceLaunch::Project {
            name: "missing".into(),
            path: path.join(format!("boomux-missing-project-{}", fastrand::u64(..))),
        };
        assert!(missing.working_directory().is_err());
        let file = super::WorkspaceLaunch::Project {
            name: "not a directory".into(),
            path: std::env::current_exe().unwrap(),
        };
        assert!(file.working_directory().is_err());
    }

    #[test]
    fn project_workspace_names_do_not_reuse_existing_workspaces() {
        assert_eq!(
            super::project_workspace_name("api", ["other"].into_iter()),
            "api"
        );
        assert_eq!(
            super::project_workspace_name("api", ["api", "api-2", "api-4"].into_iter()),
            "api-3"
        );
    }

    #[test]
    fn node_launches_preserve_exact_arguments_and_do_not_request_upgrades() {
        use super::WorkspaceLaunch;
        assert!(WorkspaceLaunch::Shell.command().is_none());
        assert_eq!(
            WorkspaceLaunch::AddNode.command().unwrap().1,
            ["__guided-node-add"]
        );
        let id = "node with spaces; $(touch should-not-exist)";
        assert_eq!(
            WorkspaceLaunch::UninstallNode(id.into())
                .command()
                .unwrap()
                .1,
            ["__guided-node-uninstall", id]
        );
        assert_eq!(
            WorkspaceLaunch::ReauthenticateNode(id.into())
                .command()
                .unwrap()
                .1,
            ["__guided-node-reauthenticate", id]
        );
    }

    #[test]
    fn config_editor_launch_uses_the_validated_cli_flow() {
        assert_eq!(
            super::WorkspaceLaunch::ConfigEdit.command().unwrap(),
            ("Edit Boomux config", vec!["config".into(), "edit".into()])
        );
    }

    use std::os::unix::net::UnixStream;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use boomux::protocol::{AgentState, AttachFrame};
    use gpui::{Keystroke, Modifiers};
    use libghostty_vt::key::Action as KeyAction;
    use libghostty_vt::terminal::Mode;

    use super::{
        AgentChoice, EMULATOR_QUEUE_CAPACITY, EmulatorCommand, EmulatorCore, SharedTerminal,
        agent_is_visible, apply_emulator_command, blank_screen, configure_terminal,
        distinguish_agent_rows, encode_key, encode_mouse_wheel, encode_paste, image_bgra,
        indexed_color, resynchronize_terminal_size, run_emulator, spawn_reader, start_emulator,
        terminal_profile,
    };
    use crate::theme::TerminalTheme;
    use std::sync::{Arc, mpsc};

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
    fn shared_shell_agents_keep_distinct_labels_and_lifecycle_observations() {
        let original = AgentChoice {
            run_id: "run-1".into(),
            id: "12345678-original".into(),
            shell_name: "fair-koala".into(),
            display_name: String::new(),
            workspace: "boomux-desktop".into(),
            shell_id: "shell-1".into(),
            integration: "codex".into(),
            state: AgentState::Working,
            updated_at_ms: 1,
            needs_attention: false,
            completed_attention: false,
            attention_revision: None,
        };
        let mut continuation = original.clone();
        continuation.id = "12345678-fork".into();
        continuation.state = AgentState::Idle;
        continuation.updated_at_ms = 2;
        let mut agents = vec![original.clone(), continuation];
        distinguish_agent_rows(&mut agents);
        assert_eq!(agents[0].display_name, "fair-koala · 12345678-o");
        assert_eq!(agents[1].display_name, "fair-koala · 12345678-f");
        assert_eq!(agents[0].state, AgentState::Working);
        assert_eq!(agents[1].state, AgentState::Idle);
        assert_eq!(agents[0].shell_id, agents[1].shell_id);

        // Activity ordering and refreshes must not renumber the threads.
        let expected = agents.clone();
        agents.reverse();
        distinguish_agent_rows(&mut agents);
        agents.reverse();
        assert_eq!(agents, expected);

        // Identical names in different Shells are not a shared-thread group.
        agents[1].shell_id = "shell-2".into();
        distinguish_agent_rows(&mut agents);
        assert!(
            agents
                .iter()
                .all(|agent| agent.display_name == "fair-koala")
        );
        assert_eq!(agents[0].id, original.id);
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
    fn ghostty_resize_redraw_does_not_insert_blank_history() {
        let shared = Arc::new(SharedTerminal::new(terminal_profile(6, 80, 800, 120)));
        let mut core = EmulatorCore::new(&shared, 6, 80, 800, 120).unwrap();
        for cols in [80, 40, 120, 60, 80] {
            core.apply(EmulatorCommand::Resize {
                rows: 6,
                cols,
                cell_width: 10,
                cell_height: 20,
            })
            .unwrap();
            // Kiro's width-change redraw clears scrollback and the screen,
            // then writes the complete frame inside synchronized output.
            let frame = format!(
                "\x1b[?2026h\x1b[3J\x1b[2J\x1b[H{}\x1b[?2026l",
                (0..30)
                    .map(|line| format!("row-{line:02}"))
                    .collect::<Vec<_>>()
                    .join("\r\n")
            );
            // PTY reads need not align with escape sequences or lines.
            for chunk in frame.as_bytes().chunks(7) {
                core.apply(EmulatorCommand::Output(chunk.to_vec())).unwrap();
            }
            for first in [0, 6, 12, 18, 24] {
                core.apply(EmulatorCommand::Scroll(
                    libghostty_vt::terminal::ScrollViewport::Row(first),
                ))
                .unwrap();
                let screen = core.screen().unwrap();
                assert_eq!(screen.scroll_total, 30, "width {cols}");
                for (offset, row) in screen.cells.chunks(usize::from(cols)).enumerate() {
                    let text = row
                        .iter()
                        .map(|cell| cell.text.as_str())
                        .collect::<String>();
                    assert_eq!(text.trim_end(), format!("row-{:02}", first + offset));
                }
            }
        }
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
    fn terminal_resize_coalesces_without_waiting_for_output_or_socket_capacity() {
        let (client, mut daemon) = UnixStream::pair().unwrap();
        daemon
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let shared = Arc::new(SharedTerminal::new(terminal_profile(3, 10, 100, 60)));
        shared.install_writer(&client).unwrap();
        let (sender, receiver) = mpsc::sync_channel(1);
        shared.install_emulator(sender);
        shared
            .emulator_command(EmulatorCommand::Output(vec![b'a']))
            .unwrap();
        let writer = shared.writer.lock().unwrap();
        let profile = shared.profile.lock().unwrap();
        let (done, completed) = mpsc::channel();
        let pending = shared.clone();
        let requester = std::thread::spawn(move || {
            for cols in 10..=109 {
                pending.request_resize((6, cols, cols * 10, 120)).unwrap();
            }
            let _ = done.send(());
        });
        let result = completed.recv_timeout(Duration::from_millis(250));
        drop(profile);
        drop(writer);
        requester.join().unwrap();
        assert!(
            result.is_ok(),
            "resize waited on a worker lock or full queue"
        );
        assert_eq!(
            *shared.pending_resize.lock().unwrap(),
            Some((6, 109, 1090, 120))
        );
        let mut core = EmulatorCore::new(&shared, 3, 10, 100, 60).unwrap();
        assert!(apply_emulator_command(&mut core, &shared, receiver.recv().unwrap()).unwrap());
        assert!(matches!(
            AttachFrame::read_from(&mut daemon).unwrap(),
            AttachFrame::Resize {
                rows: 6,
                cols: 109,
                pixel_width: 1090,
                pixel_height: 120
            }
        ));
        assert_eq!(
            (core.screen().unwrap().rows, core.screen().unwrap().cols),
            (6, 109)
        );
        assert!(shared.pending_resize.lock().unwrap().is_none());
        assert!(receiver.try_recv().is_err(), "resize flood grew the queue");
        shared.request_resize((3, 10, 100, 60)).unwrap();
        assert!(matches!(
            receiver.recv().unwrap(),
            EmulatorCommand::ResizeLatest
        ));
    }

    #[test]
    #[ignore = "manual native terminal replay and fullscreen resize measurement"]
    fn terminal_replay_resize_measurement() {
        let shared = Arc::new(SharedTerminal::new(terminal_profile(40, 120, 1200, 800)));
        let mut core = EmulatorCore::new(&shared, 40, 120, 1200, 800).unwrap();
        let bytes = b"\x1b[32mterminal replay fixture with colored output and normal line wrapping\x1b[0m\r\n".repeat(2048);
        let replay = std::time::Instant::now();
        core.apply(EmulatorCommand::Output(bytes.clone())).unwrap();
        let replay_elapsed = replay.elapsed();
        let resize = std::time::Instant::now();
        for (rows, cols) in [(80, 200), (40, 120), (80, 200), (40, 120)] {
            core.apply(EmulatorCommand::Resize {
                rows,
                cols,
                cell_width: 10,
                cell_height: 20,
            })
            .unwrap();
            let _ = core.screen().unwrap();
        }
        eprintln!(
            "native replay/resize: bytes={} replay_ms={:.3} resize_ms={:.3}",
            bytes.len(),
            replay_elapsed.as_secs_f64() * 1000.0,
            resize.elapsed().as_secs_f64() * 1000.0
        );
    }

    #[test]
    fn terminal_focus_does_not_wait_for_the_socket_writer_or_queue_capacity() {
        let (client, mut daemon) = UnixStream::pair().unwrap();
        daemon
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let shared = Arc::new(SharedTerminal::new(terminal_profile(3, 10, 100, 60)));
        shared.install_writer(&client).unwrap();
        let (sender, receiver) = mpsc::sync_channel(1);
        shared.install_emulator(sender);
        shared
            .emulator_command(EmulatorCommand::Output(vec![b'a']))
            .unwrap();
        let writer = shared.writer.lock().unwrap();
        let (done, completed) = mpsc::channel();
        let pending = shared.clone();
        let requester = std::thread::spawn(move || {
            for _ in 0..100 {
                pending.request_focus().unwrap();
            }
            let _ = done.send(());
        });
        let result = completed.recv_timeout(Duration::from_millis(250));
        drop(writer);
        requester.join().unwrap();
        assert!(
            result.is_ok(),
            "focus blocked the UI on the socket or full queue"
        );
        assert!(shared.pending_focus.load(Ordering::Acquire));
        let mut core = EmulatorCore::new(&shared, 3, 10, 100, 60).unwrap();
        assert!(apply_emulator_command(&mut core, &shared, receiver.recv().unwrap()).unwrap());
        assert!(matches!(
            AttachFrame::read_from(&mut daemon).unwrap(),
            AttachFrame::FocusGained
        ));
        assert!(!shared.pending_focus.load(Ordering::Acquire));
        assert!(
            receiver.try_recv().is_err(),
            "focus requests grew the full queue"
        );
        // On an idle queue, one marker wakes the worker and duplicate requests coalesce.
        shared.request_focus().unwrap();
        shared.request_focus().unwrap();
        assert!(matches!(
            receiver.recv().unwrap(),
            EmulatorCommand::FocusLatest
        ));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn terminal_full_output_queue_does_not_block_pane_cancellation_or_transport_close() {
        for cancel in [true, false] {
            let shared = Arc::new(SharedTerminal::new(terminal_profile(3, 10, 100, 60)));
            let (sender, receiver) = mpsc::sync_channel(1);
            shared.install_emulator(sender);
            shared
                .emulator_command(EmulatorCommand::Output(vec![b'a']))
                .unwrap();
            let (started, ready) = mpsc::channel();
            let producer_shared = shared.clone();
            let producer = std::thread::spawn(move || {
                started.send(()).unwrap();
                producer_shared.emulator_command(EmulatorCommand::Output(vec![b'b']))
            });
            ready.recv().unwrap();
            // Let the producer block behind the deliberately full queue.
            std::thread::sleep(Duration::from_millis(20));
            let (done, completed) = mpsc::channel();
            let cleanup_shared = shared.clone();
            let cleanup = std::thread::spawn(move || {
                if cancel {
                    cleanup_shared.cancel_emulator();
                } else {
                    cleanup_shared.close("detached");
                }
                let _ = done.send(());
            });
            let result = completed.recv_timeout(Duration::from_millis(250));
            // Always release the blocked producer, including on regression.
            drop(receiver);
            assert!(producer.join().unwrap().is_err());
            cleanup.join().unwrap();
            assert!(result.is_ok(), "cleanup waited for output queue capacity");
            assert!(shared.emulator.lock().unwrap().is_none());
        }
    }

    #[test]
    fn terminal_replay_chunking_preserves_escape_sequences_and_honors_cancellation() {
        let shared = Arc::new(SharedTerminal::new(terminal_profile(3, 80, 800, 60)));
        let mut chunked = EmulatorCore::new(&shared, 3, 80, 800, 60).unwrap();
        let whole_shared = Arc::new(SharedTerminal::new(terminal_profile(3, 80, 800, 60)));
        let mut whole = EmulatorCore::new(&whole_shared, 3, 80, 800, 60).unwrap();
        let mut bytes = vec![b'\r'; 16 * 1024 - 1];
        bytes.extend_from_slice("\x1b[31mhello 世界".as_bytes());
        whole.apply(EmulatorCommand::Output(bytes.clone())).unwrap();
        assert!(
            apply_emulator_command(&mut chunked, &shared, EmulatorCommand::Output(bytes)).unwrap()
        );
        assert_eq!(whole.screen().unwrap(), chunked.screen().unwrap());
        let before = chunked.screen().unwrap();
        shared.cancel_emulator();
        assert!(
            !apply_emulator_command(
                &mut chunked,
                &shared,
                EmulatorCommand::Output(b"discarded".to_vec())
            )
            .unwrap()
        );
        assert_eq!(before, chunked.screen().unwrap());
    }

    #[test]
    fn terminal_updates_remain_open_until_the_final_screen_is_published() {
        let shared = Arc::new(SharedTerminal::new(terminal_profile(6, 80, 800, 120)));
        start_emulator(&shared, 6, 80, 800, 120).unwrap();
        let screen = shared.screen.lock().unwrap();
        while shared.update_events.try_recv().is_ok() {}
        shared.process(b"Setup completed successfully.".to_vec());
        shared.close("detached");

        // Hold back screen publication to reproduce the UI observing closure first.
        assert!(shared.closed.load(Ordering::Acquire));
        assert!(shared.update_events.try_recv().is_ok());
        assert!(!shared.update_events.is_closed());
        drop(screen);

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !shared.update_events.is_closed() {
            assert!(
                std::time::Instant::now() < deadline,
                "worker did not finish"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(shared.update_events.try_recv().is_ok());
        let screen = shared.screen.lock().unwrap();
        let text: String = screen.cells.iter().map(|cell| cell.text.as_str()).collect();
        assert!(text.contains("Setup completed successfully."), "{text}");
    }

    #[test]
    fn detached_pane_retains_final_output_and_wakes_the_view() {
        for queued_output in [true, false] {
            for receipt in [
                "Setup completed successfully.",
                "Setup finished with failures.",
            ] {
                let shared = Arc::new(SharedTerminal::new(terminal_profile(6, 80, 800, 120)));
                let mut core = EmulatorCore::new(&shared, 6, 80, 800, 120).unwrap();
                let (sender, receiver) = mpsc::sync_channel(EMULATOR_QUEUE_CAPACITY);
                shared.install_emulator(sender);
                let output = format!("\x1b[?2026h{receipt}\r\nPress Ctrl+W to close this pane.");
                if queued_output {
                    shared.process(output.into_bytes());
                } else {
                    // A prior synchronized batch has not published its screen yet.
                    core.apply(EmulatorCommand::Output(output.into_bytes()))
                        .unwrap();
                }
                shared.close("detached");
                while shared.update_events.try_recv().is_ok() {}

                run_emulator(&mut core, &shared, receiver);

                let screen = shared.screen.lock().unwrap();
                let text: String = screen.cells.iter().map(|cell| cell.text.as_str()).collect();
                assert!(text.contains(receipt), "{text}");
                assert!(text.contains("Press Ctrl+W to close this pane."), "{text}");
                assert_eq!(*shared.status.lock().unwrap(), "detached");
                assert!(shared.update_events.try_recv().is_ok());
            }
        }
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
    fn initial_attachment_requests_redraw_without_a_window_resize() {
        let (client, mut daemon) = UnixStream::pair().unwrap();
        daemon
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let shared = Arc::new(SharedTerminal::new(terminal_profile(22, 70, 588, 374)));
        shared.install_writer(&client).unwrap();
        let reader = spawn_reader(
            boomux::client::Client::from_socket_path("/unused-desktop-test.sock".into()),
            "shell-test".into(),
            Some("run-test".into()),
            client,
            Arc::clone(&shared),
        );
        assert_eq!(
            AttachFrame::read_from(&mut daemon).unwrap(),
            AttachFrame::Resize {
                rows: 22,
                cols: 69,
                pixel_width: 588,
                pixel_height: 374
            }
        );
        assert_eq!(
            AttachFrame::read_from(&mut daemon).unwrap(),
            AttachFrame::Resize {
                rows: 22,
                cols: 70,
                pixel_width: 588,
                pixel_height: 374
            }
        );
        AttachFrame::Detached.write_to(&mut daemon).unwrap();
        reader.join().unwrap();
        assert!(shared.closed.load(Ordering::Acquire));
    }

    #[test]
    fn reconnected_attachment_refreshes_the_exact_run_and_preserves_output_order() {
        use boomux::protocol::{self, Envelope, Request, Response};
        use std::os::unix::net::UnixListener;
        use std::sync::mpsc;

        let directory = std::env::temp_dir().join(format!("desktop-redraw-{}", fastrand::u64(..)));
        std::fs::create_dir(&directory).unwrap();
        let socket = directory.join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (client, mut daemon) = UnixStream::pair().unwrap();
        daemon
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let profile = terminal_profile(22, 70, 588, 374);
        let shared = Arc::new(SharedTerminal::new(profile.clone()));
        let (commands, received) = mpsc::sync_channel(8);
        shared.install_emulator(commands);
        shared.install_writer(&client).unwrap();
        let reader = spawn_reader(
            boomux::client::Client::from_socket_path(socket),
            "shell-test".into(),
            Some("run-test".into()),
            client,
            Arc::clone(&shared),
        );
        for _ in 0..2 {
            assert!(matches!(
                AttachFrame::read_from(&mut daemon).unwrap(),
                AttachFrame::Resize { .. }
            ));
        }
        AttachFrame::Reconnect.write_to(&mut daemon).unwrap();
        assert_eq!(
            AttachFrame::read_from(&mut daemon).unwrap(),
            AttachFrame::ReconnectAck
        );
        let (mut reattached, _) = listener.accept().unwrap();
        reattached
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let request: Envelope<Request> = protocol::read_message(&mut reattached).unwrap();
        assert!(matches!(request.message, Request::Attach {
            shell_id, expected_run_id: Some(run_id), takeover: false, restart_exited: false,
            profile: requested_profile, ..
        } if shell_id == "shell-test" && run_id == "run-test" && requested_profile == profile));
        protocol::write_message(
            &mut reattached,
            &Envelope::with_version(
                request.version,
                Response::Attached {
                    token: "test-token".into(),
                    reconstruction: b"reconstructed".to_vec(),
                    warning: None,
                    profile: None,
                },
            ),
        )
        .unwrap();
        AttachFrame::Output(b"live".to_vec())
            .write_to(&mut reattached)
            .unwrap();
        for cols in [69, 70] {
            assert_eq!(
                AttachFrame::read_from(&mut reattached).unwrap(),
                AttachFrame::Resize {
                    rows: 22,
                    cols,
                    pixel_width: 588,
                    pixel_height: 374,
                }
            );
        }
        for expected in [b"reconstructed".as_slice(), b"live".as_slice()] {
            assert!(
                matches!(received.recv_timeout(Duration::from_secs(2)).unwrap(),
                EmulatorCommand::Output(bytes) if bytes == expected)
            );
        }
        AttachFrame::Detached.write_to(&mut reattached).unwrap();
        reader.join().unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn attachment_redraw_restores_geometry_changed_during_settle() {
        let (client, mut daemon) = UnixStream::pair().unwrap();
        let shared = SharedTerminal::new(terminal_profile(22, 70, 588, 374));
        shared.install_writer(&client).unwrap();
        resynchronize_terminal_size(&shared, |_| {
            *shared.profile.lock().unwrap() = terminal_profile(30, 100, 900, 600);
        })
        .unwrap();
        assert_eq!(
            AttachFrame::read_from(&mut daemon).unwrap(),
            AttachFrame::Resize {
                rows: 22,
                cols: 69,
                pixel_width: 588,
                pixel_height: 374
            }
        );
        assert_eq!(
            AttachFrame::read_from(&mut daemon).unwrap(),
            AttachFrame::Resize {
                rows: 30,
                cols: 100,
                pixel_width: 900,
                pixel_height: 600
            }
        );
    }

    #[test]
    fn attachment_redraw_handles_a_single_cell_terminal() {
        let (client, mut daemon) = UnixStream::pair().unwrap();
        let shared = SharedTerminal::new(terminal_profile(1, 1, 8, 16));
        shared.install_writer(&client).unwrap();
        resynchronize_terminal_size(&shared, |_| {}).unwrap();
        assert_eq!(
            AttachFrame::read_from(&mut daemon).unwrap(),
            AttachFrame::Resize {
                rows: 1,
                cols: 2,
                pixel_width: 8,
                pixel_height: 16
            }
        );
        assert_eq!(
            AttachFrame::read_from(&mut daemon).unwrap(),
            AttachFrame::Resize {
                rows: 1,
                cols: 1,
                pixel_width: 8,
                pixel_height: 16
            }
        );
    }
}
