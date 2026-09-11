mod boomux_settings;
mod bundle_update;
use boomux::generated_names;
mod git_panel;
mod layout;
mod layout_badge;
mod layout_persistence;
mod layout_state;
mod nodes;
mod remote;
mod runtime;
mod settings;
mod subprocess;
mod terminal;
mod theme;
mod updates;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use boomux::protocol::AgentState;
use gpui::{
    Animation, AnimationExt, App, Bounds, ClickEvent, ClipboardItem, Context, Corners, CursorStyle,
    Div, DragMoveEvent, FocusHandle, InteractiveElement, IntoElement, KeyBinding, KeyDownEvent,
    KeyUpEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, RenderImage,
    ScrollAnchor, ScrollHandle, ScrollWheelEvent, ShapedLine, SharedString, Stateful, TextRun,
    UnderlineStyle, Window, WindowBounds, WindowOptions, actions, canvas, div, ease_out_quint,
    fill, font, point, prelude::*, px, relative, rgb_to_hsla, rgba, size,
};
use layout::{Axis, Direction, Node, Rect};
use terminal::{
    AgentChoice, BoomuxOverview, ShellChoice, TerminalImagePlacement, TerminalScreen,
    TerminalSession,
};
use theme::{AppTheme, ThemeWatcher};

const TAB_BAR_HEIGHT: f32 = 40.0;
const MIN_FLOAT_WIDTH: f32 = 220.0;
const MIN_FLOAT_HEIGHT: f32 = 160.0;
const TERMINAL_CELL_WIDTH: f32 = 8.4;
const TERMINAL_CELL_HEIGHT: f32 = 17.0;
const TERMINAL_PADDING: f32 = 16.0;
const SIDEBAR_WIDTH: f32 = 300.0;
const DRAWER_ANIMATION_DURATION: Duration = Duration::from_millis(180);
const SCROLLBAR_FADE_IN_DURATION: Duration = Duration::from_millis(180);
const SCROLLBAR_FADE_OUT_DURATION: Duration = Duration::from_millis(360);
const DRAG_ACTIVATION_DISTANCE: f32 = 4.0;

fn rgb(color: u32) -> gpui::Rgba {
    gpui::rgb(theme::resolve_legacy(color))
}

struct HeaderTooltip(&'static str);

fn sidebar_brand_mark() -> impl IntoElement {
    div()
        .size(px(32.0))
        .flex_none()
        .rounded(px(9.0))
        .bg(rgb(0x89b4fa))
        .child(
            canvas(
                |_, _, _| (),
                |bounds, (), window, _| {
                    // Vector strokes stay crisp at small sizes and do not
                    // depend on the user's font or symbol coverage.
                    let mut prompt = gpui::PathBuilder::stroke(px(2.5));
                    let at = |x, y| point(bounds.left() + px(x), bounds.top() + px(y));
                    prompt.move_to(at(8.0, 10.0));
                    prompt.line_to(at(14.0, 16.0));
                    prompt.line_to(at(8.0, 22.0));
                    prompt.move_to(at(17.0, 22.0));
                    prompt.line_to(at(24.0, 22.0));
                    if let Ok(path) = prompt.build() {
                        window.paint_path(path, rgb(0x11111b));
                    }
                },
            )
            .size_full(),
        )
}

impl Render for HeaderTooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(rgb(0x45475a))
            .bg(rgb(0x1e1e2e))
            .text_sm()
            .text_color(rgb(0xcdd6f4))
            .child(self.0)
    }
}

// Keep the collapsed edge reachable while retaining the normal readable width.
fn sidebar_drag_target(pointer_x: f32) -> Option<f32> {
    (pointer_x > 48.0).then(|| pointer_x.clamp(280.0, 600.0))
}

fn sidebar_header_button(
    id: &'static str,
    label: &'static str,
    glyph: &'static str,
    active: bool,
) -> Stateful<Div> {
    div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label(label)
        .tooltip(move |_, cx| cx.new(|_| HeaderTooltip(label)).into())
        .size(px(28.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .border_1()
        .border_color(rgb(if active { 0xcba6f7 } else { 0x45475a }))
        .cursor_pointer()
        .text_color(rgb(0xa6adc8))
        .hover(|button| button.bg(rgb(0x313244)))
        .child(glyph)
}

fn sidebar_menu_row(id: impl Into<gpui::ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        .role(gpui::Role::MenuItem)
        .h(px(36.0))
        .px_3()
        .flex()
        .items_center()
        .rounded_md()
        .cursor_pointer()
        .hover(|row| row.bg(rgb(0x313244)))
}

actions!(
    compositor,
    [
        FocusLeft,
        FocusRight,
        FocusUp,
        FocusDown,
        MoveLeft,
        MoveRight,
        MoveUp,
        MoveDown,
        ResizeLeft,
        ResizeRight,
        ResizeUp,
        ResizeDown,
        ResizeSmallLeft,
        ResizeSmallRight,
        ResizeSmallUp,
        ResizeSmallDown,
        ResizeLargeLeft,
        ResizeLargeRight,
        ResizeLargeUp,
        ResizeLargeDown,
        ToggleSplit,
        EqualizeSplit,
        SwapSplit,
        AlignFloatingLeft,
        AlignFloatingRight,
        AlignFloatingUp,
        AlignFloatingDown,
        CenterFloating,
        CyclePaneNext,
        CyclePanePrevious,
        CycleWorkspaceNext,
        CycleWorkspacePrevious,
        NewPane,
        ClosePane,
        ToggleFloating,
        ToggleFullscreen,
        ToggleSidebarDrawer,
        ToggleSidebarFocus,
        ToggleHelp,
        ToggleGitPanel,
        RenameResource,
        RemoveShell,
        CopySelection,
        PasteClipboard,
        ToggleLayoutMode,
        ExitLayoutMode,
    ]
);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum NavigationRegion {
    #[default]
    Terminal,
    Sidebar,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FloatingAlignment {
    Left,
    Right,
    Up,
    Down,
    Center,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum SidebarItem {
    Workspace(String),
    Shell {
        workspace_id: String,
        shell_id: String,
    },
    Agent {
        agent_id: String,
        shell_id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SidebarResource {
    Workspace {
        id: String,
        name: String,
    },
    Shell {
        id: String,
        workspace_id: String,
        name: String,
    },
}

impl SidebarResource {
    fn name(&self) -> &str {
        match self {
            Self::Workspace { name, .. } | Self::Shell { name, .. } => name,
        }
    }

    fn kind_label(&self) -> &'static str {
        match self {
            Self::Workspace { .. } => "Workspace",
            Self::Shell { .. } => "Shell",
        }
    }
}

#[derive(Clone, Debug)]
struct SidebarMenu {
    target: SidebarResource,
    top: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResourceDialogKind {
    Rename,
    Remove,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShortcutSection {
    General,
    Navigation,
    Panes,
    Terminal,
    Sidebar,
}

impl ShortcutSection {
    fn label(self) -> &'static str {
        match self {
            Self::General => "GENERAL",
            Self::Navigation => "NAVIGATION",
            Self::Panes => "PANES",
            Self::Terminal => "TERMINAL",
            Self::Sidebar => "SIDEBAR",
        }
    }
}

struct ShortcutSpec {
    section: ShortcutSection,
    keys: &'static str,
    description: &'static str,
}

const KEY_TOGGLE_HELP: &str = "f1";
const KEY_RENAME_RESOURCE: &str = "f2";
const KEY_TOGGLE_SIDEBAR: &str = "f6";
const KEY_NEW_PANE: &str = "secondary-enter";
const KEY_DETACH_PANE: &str = "secondary-w";
const KEY_REMOVE_SHELL: &str = "secondary-shift-w";
fn layout_leader_starts_press(is_repeat: bool, already_pressed: bool) -> bool {
    !is_repeat && !already_pressed
}
const LAYOUT_HOLD_THRESHOLD: Duration = Duration::from_millis(250);
const LAYOUT_RELEASE_SETTLE: Duration = Duration::from_millis(50);
fn layout_leader_release_exits(entered_on_press: bool, elapsed: Duration) -> bool {
    entered_on_press && elapsed >= LAYOUT_HOLD_THRESHOLD
}
const LAYOUT_LEADER_PASSTHROUGH_WINDOW: Duration = Duration::from_millis(500);

const HELP_SHORTCUTS: &[ShortcutSpec] = &[
    ShortcutSpec {
        section: ShortcutSection::Navigation,
        keys: "Layout: G",
        description: "Select Git branches and worktrees tab",
    },
    ShortcutSpec {
        section: ShortcutSection::General,
        keys: "F1",
        description: "Toggle this help menu",
    },
    ShortcutSpec {
        section: ShortcutSection::General,
        keys: "Escape",
        description: "Close the current menu or dialog",
    },
    ShortcutSpec {
        section: ShortcutSection::Navigation,
        keys: "Ctrl + Space",
        description: "Tap to toggle Layout; hold for temporary Layout; double-tap to pass through",
    },
    ShortcutSpec {
        section: ShortcutSection::Navigation,
        keys: "F6",
        description: "Toggle focus between sidebar and terminal",
    },
    ShortcutSpec {
        section: ShortcutSection::Navigation,
        keys: "Layout: Arrow keys",
        description: "Focus an adjacent pane",
    },
    ShortcutSpec {
        section: ShortcutSection::Navigation,
        keys: "Layout: Tab / Shift + Tab",
        description: "Cycle panes forward or backward",
    },
    ShortcutSpec {
        section: ShortcutSection::Navigation,
        keys: "Layout: Page Up / Page Down",
        description: "Cycle Workspaces in sidebar order",
    },
    ShortcutSpec {
        section: ShortcutSection::Navigation,
        keys: "Layout: B",
        description: "Open or close the sidebar drawer",
    },
    ShortcutSpec {
        section: ShortcutSection::Panes,
        keys: if cfg!(target_os = "macos") {
            "Command + Enter"
        } else {
            "Ctrl + Enter"
        },
        description: "Create a Shell in the focused terminal's Workspace",
    },
    ShortcutSpec {
        section: ShortcutSection::Panes,
        keys: if cfg!(target_os = "macos") {
            "Command + W"
        } else {
            "Ctrl + W"
        },
        description: "Minimize and detach; preserve its Boomux Shell",
    },
    ShortcutSpec {
        section: ShortcutSection::Panes,
        keys: if cfg!(target_os = "macos") {
            "Command + Shift + W"
        } else {
            "Ctrl + Shift + W"
        },
        description: "Permanently remove the selected Shell",
    },
    ShortcutSpec {
        section: ShortcutSection::Panes,
        keys: "Layout: O",
        description: "Toggle tiled or floating",
    },
    ShortcutSpec {
        section: ShortcutSection::Panes,
        keys: "Layout: F",
        description: "Maximize within the workspace",
    },
    ShortcutSpec {
        section: ShortcutSection::Panes,
        keys: "Layout: Shift + Arrow / H J K L",
        description: "Move or swap the focused pane",
    },
    ShortcutSpec {
        section: ShortcutSection::Panes,
        keys: "Layout: Alt + H J K L",
        description: "Resize the focused pane precisely",
    },
    ShortcutSpec {
        section: ShortcutSection::Panes,
        keys: "Layout: Alt + Arrow",
        description: "Resize the focused pane",
    },
    ShortcutSpec {
        section: ShortcutSection::Panes,
        keys: "Layout: Alt + Shift + H J K L",
        description: "Resize the focused pane by a large step",
    },
    ShortcutSpec {
        section: ShortcutSection::Panes,
        keys: "Layout: J (or S)",
        description: "Toggle nearest split horizontal/vertical",
    },
    ShortcutSpec {
        section: ShortcutSection::Panes,
        keys: "Layout: E / R",
        description: "Equalize or swap the nearest split",
    },
    ShortcutSpec {
        section: ShortcutSection::Panes,
        keys: "Layout: Alt + Shift + Arrow",
        description: "Align a floating pane to a canvas edge",
    },
    ShortcutSpec {
        section: ShortcutSection::Panes,
        keys: "Layout: C",
        description: "Center a floating pane",
    },
    ShortcutSpec {
        section: ShortcutSection::Panes,
        keys: "Ctrl + left drag",
        description: "Lift, move, and re-tile a pane",
    },
    ShortcutSpec {
        section: ShortcutSection::Panes,
        keys: "Ctrl + right drag",
        description: "Resize a pane from the pointer",
    },
    ShortcutSpec {
        section: ShortcutSection::Terminal,
        keys: if cfg!(target_os = "macos") {
            "Command + C / V"
        } else {
            "Ctrl + Shift + C / V"
        },
        description: "Copy selection or paste clipboard",
    },
    ShortcutSpec {
        section: ShortcutSection::Terminal,
        keys: "Shift + Page Up / Down",
        description: "Scroll terminal history by one viewport",
    },
    ShortcutSpec {
        section: ShortcutSection::Terminal,
        keys: "Shift + Home / End",
        description: "Jump to the start or end of terminal history",
    },
    ShortcutSpec {
        section: ShortcutSection::Terminal,
        keys: "Left drag",
        description: "Select terminal text",
    },
    ShortcutSpec {
        section: ShortcutSection::Terminal,
        keys: "Middle click",
        description: "Paste the primary selection",
    },
    ShortcutSpec {
        section: ShortcutSection::Sidebar,
        keys: "Up / Down or J / K",
        description: "Move through visible rows",
    },
    ShortcutSpec {
        section: ShortcutSection::Sidebar,
        keys: "Left / Right or H / L",
        description: "Collapse or expand a Workspace",
    },
    ShortcutSpec {
        section: ShortcutSection::Sidebar,
        keys: "Enter",
        description: "Open the selected Workspace or Shell",
    },
    ShortcutSpec {
        section: ShortcutSection::Sidebar,
        keys: "Space",
        description: "Collapse or expand the selected Workspace",
    },
    ShortcutSpec {
        section: ShortcutSection::Sidebar,
        keys: "Tab / Shift + Tab",
        description: "Move between Workspaces and Agents",
    },
    ShortcutSpec {
        section: ShortcutSection::Sidebar,
        keys: if cfg!(target_os = "macos") {
            "Command + Enter"
        } else {
            "Ctrl + Enter"
        },
        description: "Create a Shell in the selected row's Workspace",
    },
    ShortcutSpec {
        section: ShortcutSection::Sidebar,
        keys: if cfg!(target_os = "macos") {
            "Command + Shift + Up / Down"
        } else {
            "Ctrl + Shift + Up / Down"
        },
        description: "Move the selected Workspace",
    },
    ShortcutSpec {
        section: ShortcutSection::Sidebar,
        keys: "Left drag",
        description: "Reorder a Workspace row",
    },
    ShortcutSpec {
        section: ShortcutSection::Sidebar,
        keys: "F2",
        description: "Rename the selected Workspace or Shell",
    },
];

#[derive(Clone, Debug)]
struct ResourceDialog {
    kind: ResourceDialogKind,
    target: SidebarResource,
    value: String,
    busy: bool,
    error: Option<String>,
}

fn visible_sidebar_items(
    overview: &BoomuxOverview,
    expanded_workspaces: &HashSet<String>,
) -> Vec<SidebarItem> {
    let mut items = Vec::new();
    for workspace in &overview.workspaces {
        items.push(SidebarItem::Workspace(workspace.id.clone()));
        if expanded_workspaces.contains(&workspace.id) {
            items.extend(workspace.shells.iter().map(|shell| SidebarItem::Shell {
                workspace_id: workspace.id.clone(),
                shell_id: shell.id.clone(),
            }));
        }
    }
    items.extend(overview.agents.iter().map(|agent| SidebarItem::Agent {
        agent_id: agent.id.clone(),
        shell_id: agent.shell_id.clone(),
    }));
    items
}

fn reconcile_completed_agents(
    previous_states: &mut HashMap<String, AgentState>,
    completed_agents: &mut HashSet<String>,
    agents: &[AgentChoice],
) {
    let current_idle_agents = agents
        .iter()
        .filter(|agent| agent.state == AgentState::Idle)
        .map(|agent| agent.id.clone())
        .collect::<HashSet<_>>();
    completed_agents.retain(|agent_id| current_idle_agents.contains(agent_id));

    for agent in agents {
        if agent.state == AgentState::Idle
            && previous_states.get(&agent.id) == Some(&AgentState::Working)
        {
            completed_agents.insert(agent.id.clone());
        }
    }

    previous_states.clear();
    previous_states.extend(agents.iter().map(|agent| (agent.id.clone(), agent.state)));
}

fn reconcile_workspace_order(order: &mut Vec<String>, overview: &mut BoomuxOverview) {
    let known = overview
        .workspaces
        .iter()
        .map(|workspace| workspace.id.clone())
        .collect::<HashSet<_>>();
    order.retain(|workspace_id| known.contains(workspace_id));
    for workspace in &overview.workspaces {
        if !order.contains(&workspace.id) {
            order.push(workspace.id.clone());
        }
    }
    overview.workspaces.sort_by_key(|workspace| {
        order
            .iter()
            .position(|workspace_id| workspace_id == &workspace.id)
            .unwrap_or(usize::MAX)
    });
}

fn reorder_workspace(order: &mut Vec<String>, source: &str, target: &str, after: bool) -> bool {
    if source == target {
        return false;
    }
    let Some(source_index) = order.iter().position(|id| id == source) else {
        return false;
    };
    let previous = order.clone();
    let source_id = order.remove(source_index);
    let Some(target_index) = order.iter().position(|id| id == target) else {
        *order = previous;
        return false;
    };
    order.insert(target_index + usize::from(after), source_id);
    *order != previous
}

const SIDEBAR_WORKSPACE_HEADER_HEIGHT: f32 = 52.0;
const SIDEBAR_SHELL_ROW_HEIGHT: f32 = 39.0;

fn sidebar_workspace_height(
    workspace: &terminal::WorkspaceChoice,
    expanded_workspaces: &HashSet<String>,
    pane_layout_mode: PaneLayoutMode,
) -> f32 {
    let shell_height = if pane_layout_mode != PaneLayoutMode::Tabbed
        && expanded_workspaces.contains(&workspace.id)
    {
        workspace.shells.len() as f32 * SIDEBAR_SHELL_ROW_HEIGHT
    } else {
        0.0
    };
    SIDEBAR_WORKSPACE_HEADER_HEIGHT + shell_height
}

fn sidebar_workspace_offsets(
    overview: &BoomuxOverview,
    expanded_workspaces: &HashSet<String>,
    pane_layout_mode: PaneLayoutMode,
) -> HashMap<String, f32> {
    let mut y = 0.0;
    overview
        .workspaces
        .iter()
        .map(|workspace| {
            let offset = (workspace.id.clone(), y);
            y += sidebar_workspace_height(workspace, expanded_workspaces, pane_layout_mode);
            offset
        })
        .collect()
}

fn sidebar_workspace_drop_target(
    overview: &BoomuxOverview,
    expanded_workspaces: &HashSet<String>,
    pane_layout_mode: PaneLayoutMode,
    pointer_y: f32,
) -> Option<(String, bool)> {
    let first = overview.workspaces.first()?;
    if pointer_y <= 0.0 {
        return Some((first.id.clone(), false));
    }

    let mut y = 0.0;
    for workspace in &overview.workspaces {
        let height = sidebar_workspace_height(workspace, expanded_workspaces, pane_layout_mode);
        if pointer_y < y + height {
            return Some((workspace.id.clone(), pointer_y >= y + height / 2.0));
        }
        y += height;
    }

    overview
        .workspaces
        .last()
        .map(|workspace| (workspace.id.clone(), true))
}

fn sidebar_item_visible_in_layout(mode: PaneLayoutMode, item: &SidebarItem) -> bool {
    mode != PaneLayoutMode::Tabbed || !matches!(item, SidebarItem::Shell { .. })
}

fn reconciled_sidebar_item(
    current: Option<&SidebarItem>,
    preferred: Option<&SidebarItem>,
    visible: &[SidebarItem],
) -> Option<SidebarItem> {
    current
        .filter(|item| visible.contains(item))
        .or_else(|| preferred.filter(|item| visible.contains(item)))
        .or_else(|| visible.first())
        .cloned()
}

fn workspace_id_for_sidebar_item(item: &SidebarItem, overview: &BoomuxOverview) -> Option<String> {
    match item {
        SidebarItem::Workspace(workspace_id) => overview
            .workspaces
            .iter()
            .find(|workspace| workspace.id == *workspace_id)
            .map(|workspace| workspace.id.clone()),
        SidebarItem::Shell {
            workspace_id,
            shell_id,
        } => overview
            .workspaces
            .iter()
            .find(|workspace| {
                workspace.id == *workspace_id
                    && workspace.shells.iter().any(|shell| shell.id == *shell_id)
            })
            .map(|workspace| workspace.id.clone()),
        SidebarItem::Agent { agent_id, shell_id } => {
            let agent = overview
                .agents
                .iter()
                .find(|agent| agent.id == *agent_id && agent.shell_id == *shell_id)?;
            overview.workspaces.iter().find_map(|workspace| {
                workspace
                    .shells
                    .iter()
                    .any(|shell| shell.id == agent.shell_id)
                    .then(|| workspace.id.clone())
            })
        }
    }
}

#[derive(Clone, Debug)]
struct FloatingPane {
    id: usize,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

#[derive(Clone, Debug)]
struct FloatingAnimation {
    pane_id: usize,
    from: FloatingPane,
    generation: u64,
}

#[derive(Clone, Debug)]
struct PaneMinimizeAnimation {
    pane_id: usize,
    from: FloatingPane,
    generation: u64,
    duration: Duration,
}

#[derive(Clone, Debug)]
struct WorkspaceTransition {
    outgoing: Vec<FloatingPane>,
    direction: f32,
    generation: u64,
    duration: Duration,
}

#[derive(Clone, Copy, Debug)]
enum PointerOperation {
    Move,
    Resize,
    ResizeEdge(Direction),
    ResizeCorner(Direction, Direction),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum PaneCornerStyle {
    #[default]
    Rounded,
    Square,
    Mixed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum WorkspacePaneMode {
    #[default]
    Workspace,
    Mixed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum PaneLayoutMode {
    Tiled,
    #[default]
    Tabbed,
}

fn pane_layout_supports_scope(layout: PaneLayoutMode, scope: WorkspacePaneMode) -> bool {
    layout != PaneLayoutMode::Tabbed || scope == WorkspacePaneMode::Workspace
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShellPanePresence {
    Focused,
    Open,
    Minimized,
}

impl ShellPanePresence {
    fn glyph(self) -> &'static str {
        match self {
            Self::Focused => "●",
            Self::Open => "◉",
            Self::Minimized => "○",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Focused | Self::Open => "open",
            Self::Minimized => "minimized",
        }
    }
}

fn shell_pane_presence(focused: bool, open: bool) -> ShellPanePresence {
    if focused {
        ShellPanePresence::Focused
    } else if open {
        ShellPanePresence::Open
    } else {
        ShellPanePresence::Minimized
    }
}

fn workspace_open_replaces_panes(
    mode: WorkspacePaneMode,
    current: &HashSet<String>,
    desired: &HashSet<String>,
) -> bool {
    mode == WorkspacePaneMode::Workspace && current != desired
}

fn shell_open_replaces_panes(
    mode: WorkspacePaneMode,
    open_workspace_ids: &HashSet<String>,
    target_workspace_id: &str,
) -> bool {
    mode == WorkspacePaneMode::Workspace
        && open_workspace_ids
            .iter()
            .any(|workspace_id| workspace_id != target_workspace_id)
}

fn shell_is_minimized(minimized_shells: &HashSet<String>, shell_id: &str) -> bool {
    minimized_shells.contains(shell_id)
}

fn reveal_opened_workspace(
    mode: WorkspacePaneMode,
    expanded_workspaces: &mut HashSet<String>,
    workspace_id: &str,
) {
    if mode == WorkspacePaneMode::Workspace {
        expanded_workspaces.clear();
    }
    expanded_workspaces.insert(workspace_id.to_string());
}

fn workspace_slide_direction(
    workspace_order: &[String],
    current_workspace_id: Option<&str>,
    target_workspace_id: &str,
) -> f32 {
    let current_index = current_workspace_id
        .and_then(|id| workspace_order.iter().position(|workspace| workspace == id));
    let target_index = workspace_order
        .iter()
        .position(|workspace| workspace == target_workspace_id);
    match (current_index, target_index) {
        (Some(current), Some(target)) if target < current => -1.0,
        _ => 1.0,
    }
}

fn cycled_workspace_id<'a>(
    workspace_order: &'a [String],
    current_workspace_id: Option<&str>,
    backwards: bool,
) -> Option<&'a str> {
    let count = workspace_order.len();
    if count == 0 {
        return None;
    }
    let Some(current) = current_workspace_id
        .and_then(|id| workspace_order.iter().position(|workspace| workspace == id))
    else {
        return workspace_order
            .get(if backwards { count - 1 } else { 0 })
            .map(String::as_str);
    };
    let next = if backwards {
        (current + count - 1) % count
    } else {
        (current + 1) % count
    };
    workspace_order.get(next).map(String::as_str)
}

fn shifted_workspace_rect(rect: Rect, direction: f32) -> Rect {
    Rect {
        x: rect.x + direction,
        ..rect
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum MotionSpeed {
    Instant,
    Fast,
    #[default]
    Smooth,
}

impl MotionSpeed {
    fn duration(self) -> Option<Duration> {
        match self {
            Self::Instant => None,
            Self::Fast => Some(Duration::from_millis(180)),
            Self::Smooth => Some(Duration::from_millis(360)),
        }
    }
}

#[derive(Clone, Debug)]
struct PointerDrag {
    operation: PointerOperation,
    button: MouseButton,
    start_pointer: (f32, f32),
    pane_id: usize,
    subject: PointerSubject,
    activated: bool,
}

#[derive(Clone, Debug)]
struct LayoutAnimation {
    from: HashMap<usize, Rect>,
    generation: u64,
    paint_last: Option<usize>,
}

#[derive(Clone, Debug)]
struct WorkspaceOrderAnimation {
    from: HashMap<String, f32>,
    generation: u64,
}

#[derive(Clone, Debug)]
enum PointerSubject {
    Floating(FloatingPane),
    Lifted(FloatingPane),
    Tiled(Node),
}

#[derive(Clone, Debug)]
struct TerminalScrollbarPointerDrag {
    pane_id: usize,
    start_pointer_y: f32,
    start_offset: usize,
    maximum_offset: usize,
    travel_height: f32,
}

#[derive(Clone)]
struct TerminalSelectionDrag {
    pane_id: usize,
}

#[derive(Clone)]
struct WorkspaceRowDrag {
    workspace_id: String,
    workspace_name: String,
}

impl Render for WorkspaceRowDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .w(px(270.0))
            .h(px(48.0))
            .px_3()
            .flex()
            .items_center()
            .gap_2()
            .rounded_md()
            .border_1()
            .border_color(rgb(0xcba6f7))
            .bg(rgb(0x252536))
            .shadow_lg()
            .child(div().size_2().rounded_full().bg(rgb(0x89b4fa)))
            .child(
                div()
                    .text_sm()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(rgb(0xcdd6f4))
                    .child(self.workspace_name.clone()),
            )
    }
}

impl TerminalSelectionDrag {
    fn selection(
        &self,
        pane_id: usize,
        anchor: Option<gpui::Point<gpui::Pixels>>,
        position: gpui::Point<gpui::Pixels>,
        bounds: Bounds<gpui::Pixels>,
        screen: &TerminalScreen,
    ) -> Option<TerminalSelection> {
        // GPUI dispatches drag moves to every listener of this type, including
        // other panes. Only the source pane's bounds describe this selection.
        if pane_id != self.pane_id {
            return None;
        }
        let cell = |position: gpui::Point<gpui::Pixels>| {
            terminal_cell_from_offset(
                f32::from(position.x - bounds.left()),
                f32::from(position.y - bounds.top()),
                screen,
            )
        };
        Some(TerminalSelection {
            anchor: cell(anchor?),
            head: cell(position),
        })
    }
}

impl Render for TerminalSelectionDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TerminalSelection {
    anchor: (usize, usize),
    head: (usize, usize),
}

fn lifted_drag_bounds(mut bounds: FloatingPane, delta: (f32, f32)) -> FloatingPane {
    // A tiled pane may span the entire panel. Keep the grab point under the
    // pointer even when part of the temporary floating pane leaves the panel.
    bounds.x += delta.0;
    bounds.y += delta.1;
    bounds
}

fn dragged_bounds(
    mut bounds: FloatingPane,
    operation: PointerOperation,
    delta: (f32, f32),
    panel_size: (f32, f32),
) -> FloatingPane {
    match operation {
        PointerOperation::Move => {
            bounds.x = (bounds.x + delta.0).clamp(0.0, (panel_size.0 - bounds.width).max(0.0));
            bounds.y = (bounds.y + delta.1).clamp(0.0, (panel_size.1 - bounds.height).max(0.0));
        }
        PointerOperation::ResizeCorner(horizontal, vertical) => {
            bounds = dragged_bounds(
                bounds,
                PointerOperation::ResizeEdge(horizontal),
                delta,
                panel_size,
            );
            bounds = dragged_bounds(
                bounds,
                PointerOperation::ResizeEdge(vertical),
                delta,
                panel_size,
            );
        }
        PointerOperation::ResizeEdge(edge) => {
            let right = bounds.x + bounds.width;
            let bottom = bounds.y + bounds.height;
            match edge {
                Direction::Left => {
                    bounds.x = (bounds.x + delta.0).clamp(0.0, (right - MIN_FLOAT_WIDTH).max(0.0));
                    bounds.width = right - bounds.x;
                }
                Direction::Up => {
                    bounds.y =
                        (bounds.y + delta.1).clamp(0.0, (bottom - MIN_FLOAT_HEIGHT).max(0.0));
                    bounds.height = bottom - bounds.y;
                }
                Direction::Right => {
                    let available = (panel_size.0 - bounds.x).max(0.0);
                    bounds.width =
                        (bounds.width + delta.0).clamp(MIN_FLOAT_WIDTH.min(available), available);
                }
                Direction::Down => {
                    let available = (panel_size.1 - bounds.y).max(0.0);
                    bounds.height =
                        (bounds.height + delta.1).clamp(MIN_FLOAT_HEIGHT.min(available), available);
                }
            }
        }
        PointerOperation::Resize => {
            let available_width = (panel_size.0 - bounds.x).max(0.0);
            let available_height = (panel_size.1 - bounds.y).max(0.0);
            bounds.width = (bounds.width + delta.0)
                .clamp(MIN_FLOAT_WIDTH.min(available_width), available_width);
            bounds.height = (bounds.height + delta.1)
                .clamp(MIN_FLOAT_HEIGHT.min(available_height), available_height);
        }
    }
    bounds
}

fn clamp_floating_to_panel(mut bounds: FloatingPane, panel_size: (f32, f32)) -> FloatingPane {
    bounds.width = bounds.width.min(panel_size.0);
    bounds.height = bounds.height.min(panel_size.1);
    bounds.x = bounds.x.clamp(0.0, (panel_size.0 - bounds.width).max(0.0));
    bounds.y = bounds.y.clamp(0.0, (panel_size.1 - bounds.height).max(0.0));
    bounds
}

fn align_floating_to_panel(
    bounds: FloatingPane,
    alignment: FloatingAlignment,
    panel_size: (f32, f32),
    pane_gap: f32,
) -> FloatingPane {
    let mut bounds = clamp_floating_to_panel(bounds, panel_size);
    let max_x = (panel_size.0 - bounds.width).max(0.0);
    let max_y = (panel_size.1 - bounds.height).max(0.0);
    let left = pane_gap.min(max_x);
    let right = (max_x - pane_gap).max(0.0);
    let top = pane_gap.min(max_y);
    let bottom = (max_y - pane_gap).max(0.0);
    match alignment {
        FloatingAlignment::Left => bounds.x = left,
        FloatingAlignment::Right => bounds.x = right,
        FloatingAlignment::Up => bounds.y = top,
        FloatingAlignment::Down => bounds.y = bottom,
        FloatingAlignment::Center => {
            bounds.x = max_x / 2.0;
            bounds.y = max_y / 2.0;
        }
    }
    bounds
}

fn resize_floating_in_direction(
    bounds: FloatingPane,
    direction: Direction,
    amount: f32,
    panel_size: (f32, f32),
) -> FloatingPane {
    let mut bounds = clamp_floating_to_panel(bounds, panel_size);
    match direction {
        Direction::Left => bounds.width = (bounds.width - amount).max(MIN_FLOAT_WIDTH),
        Direction::Right => {
            let delta = amount.min((panel_size.0 - bounds.x - bounds.width).max(0.0));
            bounds.width += delta;
        }
        Direction::Up => bounds.height = (bounds.height - amount).max(MIN_FLOAT_HEIGHT),
        Direction::Down => {
            let delta = amount.min((panel_size.1 - bounds.y - bounds.height).max(0.0));
            bounds.height += delta;
        }
    }
    bounds
}

fn cycled_pane_id(ids: &[usize], focused: usize, backwards: bool) -> Option<usize> {
    if ids.is_empty() {
        return None;
    }
    let current = ids.iter().position(|id| *id == focused).unwrap_or(0);
    let next = if backwards {
        current.checked_sub(1).unwrap_or(ids.len() - 1)
    } else {
        (current + 1) % ids.len()
    };
    Some(ids[next])
}

fn ordered_pane_ids(layout: Option<&Node>, floating: &[FloatingPane]) -> Vec<usize> {
    let mut ids = layout.map_or_else(Vec::new, Node::pane_ids);
    let mut floating_ids = floating.iter().map(|pane| pane.id).collect::<Vec<_>>();
    floating_ids.sort_unstable();
    ids.extend(floating_ids);
    ids
}

fn centered_floating_pane(id: usize, panel_size: (f32, f32), pane_gap: f32) -> FloatingPane {
    let margin = pane_gap.max(24.0);
    let available_width = (panel_size.0 - margin * 2.0).max(1.0);
    let available_height = (panel_size.1 - margin * 2.0).max(1.0);
    let width = (panel_size.0 * 0.7)
        .clamp(MIN_FLOAT_WIDTH, 1_100.0)
        .min(available_width);
    let height = (panel_size.1 * 0.72)
        .clamp(MIN_FLOAT_HEIGHT, 760.0)
        .min(available_height);
    FloatingPane {
        id,
        x: (panel_size.0 - width) / 2.0,
        y: (panel_size.1 - height) / 2.0,
        width,
        height,
    }
}

fn workspace_maximized_pane(id: usize, panel_size: (f32, f32), pane_gap: f32) -> FloatingPane {
    let (inner_width, inner_height) = inset_panel_size(panel_size, pane_gap);
    FloatingPane {
        id,
        x: pane_gap,
        y: pane_gap,
        width: (inner_width - pane_gap).max(1.0),
        height: (inner_height - pane_gap).max(1.0),
    }
}

fn interpolate_floating_pane(
    from: &FloatingPane,
    to: &FloatingPane,
    progress: f32,
) -> FloatingPane {
    let lerp = |start: f32, end: f32| start + (end - start) * progress;
    FloatingPane {
        id: to.id,
        x: lerp(from.x, to.x),
        y: lerp(from.y, to.y),
        width: lerp(from.width, to.width),
        height: lerp(from.height, to.height),
    }
}

fn inset_panel_size(panel_size: (f32, f32), inset: f32) -> (f32, f32) {
    (
        (panel_size.0 - inset * 2.0).max(1.0),
        (panel_size.1 - inset * 2.0).max(1.0),
    )
}

fn pane_corner_radii(pane_id: usize, style: PaneCornerStyle) -> [f32; 4] {
    match style {
        PaneCornerStyle::Rounded => [8.0; 4],
        PaneCornerStyle::Square => [0.0; 4],
        PaneCornerStyle::Mixed => {
            const RADII: &[f32] = &[0.0, 3.0, 7.0, 12.0, 18.0];
            let mut seed = (pane_id as u64)
                .wrapping_add(0x9e37_79b9_7f4a_7c15)
                .wrapping_mul(0xbf58_476d_1ce4_e5b9);
            let mut corners = std::array::from_fn(|_| {
                seed ^= seed >> 30;
                seed = seed.wrapping_mul(0x94d0_49bb_1331_11eb);
                seed ^= seed >> 27;
                RADII[seed as usize % RADII.len()]
            });
            let square_corner = seed as usize % corners.len();
            corners[square_corner] = 0.0;
            if corners.iter().all(|radius| *radius == 0.0) {
                corners[(square_corner + 1) % corners.len()] = 12.0;
            }
            corners
        }
    }
}

fn blend_rgb(from: u32, to: u32, strength: u8) -> u32 {
    let amount = u32::from(strength.min(100));
    let channel = |shift: u32| {
        let start = (from >> shift) & 0xff_u32;
        let end = (to >> shift) & 0xff_u32;
        (start * (100 - amount) + end * amount + 50) / 100
    };
    (channel(16) << 16) | (channel(8) << 8) | channel(0)
}

fn resized_tiled_layout(
    mut layout: Node,
    pane_id: usize,
    delta: (f32, f32),
    panel_size: (f32, f32),
) -> Node {
    layout.resize_from_pointer(pane_id, Axis::Horizontal, delta.0 / panel_size.0.max(1.0));
    layout.resize_from_pointer(pane_id, Axis::Vertical, delta.1 / panel_size.1.max(1.0));
    layout
}

fn interpolate_rect(from: Rect, to: Rect, progress: f32) -> Rect {
    let lerp = |start: f32, end: f32| start + (end - start) * progress;
    Rect {
        x: lerp(from.x, to.x),
        y: lerp(from.y, to.y),
        width: lerp(from.width, to.width),
        height: lerp(from.height, to.height),
    }
}

fn workspace_layout_rects(layout: &Node, maximized: Option<usize>) -> Vec<(usize, Rect)> {
    let mut rects = layout.rects();
    let Some(maximized) = maximized.filter(|id| layout.contains(*id)) else {
        return rects;
    };
    if let Some((_, rect)) = rects.iter_mut().find(|(id, _)| *id == maximized) {
        *rect = Rect {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
        };
    }
    // The maximized pane must paint after its tiled siblings.
    rects.sort_by_key(|(id, _)| *id == maximized);
    rects
}

fn paint_layout_pane_last(rects: &mut [(usize, Rect)], pane_id: Option<usize>) {
    if let Some(pane_id) = pane_id {
        rects.sort_by_key(|(id, _)| *id == pane_id);
    }
}

fn swap_layout_direction(
    layout: &mut Node,
    focused: usize,
    direction: Direction,
) -> Option<HashMap<usize, Rect>> {
    let neighbor = layout.neighbor(focused, direction)?;
    let previous_rects = layout.rects().into_iter().collect();
    layout.swap_panes(focused, neighbor);
    Some(previous_rects)
}

fn window_point_to_panel(x: f32, y: f32, sidebar_width: f32) -> (f32, f32) {
    (x - sidebar_width, y)
}

fn pointer_moved_from(anchor: (f32, f32), current: (f32, f32)) -> bool {
    (anchor.0 - current.0).abs() > 0.5 || (anchor.1 - current.1).abs() > 0.5
}

fn terminal_cell_from_offset(x: f32, y: f32, screen: &TerminalScreen) -> (usize, usize) {
    let col = ((x - 8.0) / TERMINAL_CELL_WIDTH).floor().max(0.0) as usize;
    let row = ((y - 8.0) / TERMINAL_CELL_HEIGHT).floor().max(0.0) as usize;
    (
        row.min(usize::from(screen.rows).saturating_sub(1)),
        col.min(usize::from(screen.cols).saturating_sub(1)),
    )
}

fn selection_indices(selection: TerminalSelection, cols: usize) -> (usize, usize) {
    let anchor = selection.anchor.0 * cols + selection.anchor.1;
    let head = selection.head.0 * cols + selection.head.1;
    (anchor.min(head), anchor.max(head))
}

fn terminal_selected_text(screen: &TerminalScreen, selection: TerminalSelection) -> String {
    let cols = usize::from(screen.cols);
    let (start, end) = selection_indices(selection, cols);
    let first_row = start / cols;
    let last_row = end / cols;
    (first_row..=last_row)
        .map(|row| {
            let start_col = if row == first_row { start % cols } else { 0 };
            let end_col = if row == last_row {
                end % cols
            } else {
                cols.saturating_sub(1)
            };
            let mut line = String::new();
            for col in start_col..=end_col {
                let cell = &screen.cells[row * cols + col];
                if !cell.continuation {
                    line.push_str(&cell.text);
                }
            }
            line.trim_end().to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// Consume a completed gesture once, even when automatic copying is disabled.
// The source pane remains authoritative if focus changed during the drag.
fn take_terminal_selection_copy(
    pending: &mut Option<usize>,
    panes: &HashMap<usize, TerminalPane>,
    enabled: bool,
) -> Option<String> {
    let pane_id = pending.take()?;
    if !enabled {
        return None;
    }
    let pane = panes.get(&pane_id)?;
    let text = terminal_selected_text(pane.screen.as_ref()?, pane.selection?);
    (!text.is_empty()).then_some(text)
}

fn drop_placement(layout: &Node, point: (f32, f32)) -> Option<(usize, Axis, bool)> {
    let (id, rect) = layout.rects().into_iter().find(|(_, rect)| {
        point.0 >= rect.x
            && point.0 <= rect.x + rect.width
            && point.1 >= rect.y
            && point.1 <= rect.y + rect.height
    })?;
    let dx = (point.0 - (rect.x + rect.width / 2.0)) / rect.width.max(f32::EPSILON);
    let dy = (point.1 - (rect.y + rect.height / 2.0)) / rect.height.max(f32::EPSILON);

    if dx.abs() >= dy.abs() {
        Some((id, Axis::Horizontal, dx < 0.0))
    } else {
        Some((id, Axis::Vertical, dy < 0.0))
    }
}

fn sidebar_render_sources(
    overview: &BoomuxOverview,
    settings_open: bool,
) -> (&[terminal::WorkspaceChoice], &[AgentChoice]) {
    if settings_open {
        (&[], &[])
    } else {
        (&overview.workspaces, &overview.agents)
    }
}

fn setup_pane_was_removed(
    shell: &ShellChoice,
    output_finished: bool,
    overview: &BoomuxOverview,
) -> bool {
    shell.desktop_setup
        && output_finished
        && !overview
            .workspaces
            .iter()
            .flat_map(|workspace| &workspace.shells)
            .any(|current| current.id == shell.id)
}

fn scrollbar_thumb_fraction(screen: &TerminalScreen, track_height: f32) -> f32 {
    if screen.scroll_total == 0 {
        return 1.0;
    }
    let visible = screen.scroll_len as f32 / screen.scroll_total as f32;
    let minimum = 20.0 / track_height.max(20.0);
    visible.max(minimum).min(1.0)
}

fn scrollbar_offset_from_drag(
    start_offset: usize,
    maximum_offset: usize,
    pointer_delta: f32,
    travel_height: f32,
) -> usize {
    if maximum_offset == 0 || travel_height <= 0.0 {
        return 0;
    }
    (start_offset as f32 + pointer_delta / travel_height * maximum_offset as f32)
        .round()
        .clamp(0.0, maximum_offset as f32) as usize
}

fn scrollbar_fade_opacity(visible: bool, progress: f32) -> f32 {
    if visible { progress } else { 1.0 - progress }
}

fn layout_leader_passes_through(elapsed: Duration) -> bool {
    elapsed <= LAYOUT_LEADER_PASSTHROUGH_WINDOW
}

fn contrast_foreground(background: u32) -> u32 {
    let red = (background >> 16) & 0xff;
    let green = (background >> 8) & 0xff;
    let blue = background & 0xff;
    if red * 299 + green * 587 + blue * 114 > 150 * 1_000 {
        0x111111
    } else {
        0xffffff
    }
}

fn workspace_key_context(
    help_open: bool,
    navigation_region: NavigationRegion,
    layout_mode: bool,
) -> &'static str {
    if help_open {
        "Help"
    } else if navigation_region == NavigationRegion::Sidebar && layout_mode {
        "SidebarLayout"
    } else if navigation_region == NavigationRegion::Sidebar {
        "Sidebar"
    } else if layout_mode {
        "Layout"
    } else {
        "Terminal"
    }
}

fn desktop_window_title(workspace_name: Option<&str>) -> String {
    workspace_name.map_or_else(
        || "Boomux Desktop".into(),
        |name| format!("Boomux Desktop — {name}"),
    )
}

struct Workspace {
    layout_document: layout_state::Document,
    layout_writer: Option<async_channel::Sender<layout_state::Write>>,
    layout_save_task: Option<gpui::Task<()>>,
    layout_error: Option<String>,
    layout_canvas: (f32, f32),
    layout_generation: u64,
    layout_frozen: bool,
    layout_closing: bool,
    restore_pointer_guard: layout_persistence::PointerGuard,
    layout_restoring: bool,
    git_panel: git_panel::Model,
    layout: Option<Node>,
    floating: Vec<FloatingPane>,
    pointer_drag: Option<PointerDrag>,
    terminal_scrollbar_drag: Option<TerminalScrollbarPointerDrag>,
    terminal_selection_release: Option<usize>,
    copied_pane: Option<usize>,
    copied_cleanup: Option<gpui::Task<()>>,
    layout_animation: Option<LayoutAnimation>,
    workspace_order_animation: Option<WorkspaceOrderAnimation>,
    floating_animation: Option<FloatingAnimation>,
    minimizing_panes: Vec<PaneMinimizeAnimation>,
    workspace_transition: Option<WorkspaceTransition>,
    animation_generation: u64,
    focused: usize,
    fullscreen: Option<usize>,
    boomux_shells: Vec<ShellChoice>,
    boomux_overview: BoomuxOverview,
    previous_agent_states: HashMap<String, AgentState>,
    completed_agents: HashSet<String>,
    dismissing_agents: HashSet<String>,
    workspace_order: Vec<String>,
    boomux_error: Option<String>,
    expanded_workspaces: HashSet<String>,
    navigation_region: NavigationRegion,
    sidebar_focus_pointer: Option<(f32, f32)>,
    sidebar_item: Option<SidebarItem>,
    sidebar_scroll_handle: ScrollHandle,
    sidebar_agent_scroll_handle: ScrollHandle,
    sidebar_agent_scroll_anchor: ScrollAnchor,
    sidebar_scroll_anchor: ScrollAnchor,
    minimized_tab_scroll_handle: ScrollHandle,
    sidebar_menu: Option<SidebarMenu>,
    sidebar_header_menu_open: bool,
    project_menu_open: bool,
    projects_loading: bool,
    projects_result: Option<Result<boomux::protocol::HostProjectDiscovery, String>>,
    project_folder_picker_pending: bool,
    project_folder_picker_open: bool,
    nodes_open: bool,
    node_views: Vec<nodes::NodeView>,
    nodes_error: Option<String>,
    node_forget_confirm: Option<String>,
    node_forget_busy: bool,
    node_forget_error: Option<String>,
    selected_node: Option<String>,
    expanded_node: Option<String>,
    resource_dialog: Option<ResourceDialog>,
    sidebar_visible: bool,
    sidebar_preferred_width: f32,
    sidebar_viewport_width: f32,
    sidebar_resizing: bool,
    sidebar_drag_width: Option<f32>,
    drawer_animation_from: Option<f32>,
    drawer_animation_generation: u64,
    pane_headings_visible: bool,
    pane_corner_style: PaneCornerStyle,
    pane_gap: f32,
    focus_highlight_strength: u8,
    motion_speed: MotionSpeed,
    layout_overlay_visible: bool,
    copy_on_select: bool,
    workspace_pane_mode: WorkspacePaneMode,
    pane_layout_mode: PaneLayoutMode,
    minimized_shells: HashSet<String>,
    confirm_destructive_actions: bool,
    theme: AppTheme,
    theme_watcher: Option<ThemeWatcher>,
    theme_load_generation: u64,
    theme_error: Option<String>,
    update_check: updates::Check,
    updates_checking: bool,
    update_busy: bool,
    prepared_update: Option<bundle_update::Prepared>,
    onboarding_complete: bool,
    updates_status: Option<String>,
    update_task: Option<gpui::Task<()>>,
    dismissed_desktop_update: String,
    dismissed_boomux_update: String,
    settings_open: bool,
    settings_writer: Option<async_channel::Sender<settings::Settings>>,
    settings_error: Option<String>,
    settings_restart_pending: bool,
    settings_restart_confirm: bool,
    boomux_settings_snapshot: Option<boomux_settings::Snapshot>,
    boomux_settings_busy: bool,
    boomux_settings_message: Option<String>,
    boomux_setting_input: Option<(usize, String)>,
    help_open: bool,
    help_scroll_handle: ScrollHandle,
    layout_mode: bool,
    layout_mode_entered_at: Option<Instant>,
    layout_leader_pressed_at: Option<Instant>,
    layout_leader_entered: bool,
    layout_leader_release_task: Option<gpui::Task<()>>,
    layout_suppressed_keys: HashSet<String>,
    layout_badge_generation: u64,
    layout_badge_exiting: bool,
    layout_badge_cleanup: Option<gpui::Task<()>>,
    terminal_pressed_keys: HashMap<String, usize>,
    terminals: HashMap<usize, TerminalPane>,
    next_id: usize,
    focus_handle: FocusHandle,
}

#[derive(Default)]
struct TerminalPane {
    shell: Option<ShellChoice>,
    restored: Option<layout_state::Pane>,
    restore_attempt: Option<String>,
    restore_retry_after: Option<Instant>,
    restore_failures: u8,
    session: Option<TerminalSession>,
    screen: Option<Arc<TerminalScreen>>,
    attaching: bool,
    error: Option<String>,
    scroll_remainder: f32,
    scrollbar_hovered: bool,
    scrollbar_fade_generation: u64,
    selection: Option<TerminalSelection>,
    selection_anchor_position: Option<gpui::Point<gpui::Pixels>>,
    render_images: HashMap<u64, Arc<RenderImage>>,
    render_image_screen: Option<Arc<TerminalScreen>>,
    paint_cache: Option<Arc<TerminalPaintCache>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TerminalBackground {
    row: usize,
    col: usize,
    color: u32,
}

struct TerminalPaintCache {
    screen: Arc<TerminalScreen>,
    selection: Option<TerminalSelection>,
    lines: Vec<ShapedLine>,
    backgrounds: Vec<TerminalBackground>,
    cursor_focused: bool,
    cursor_outline: Option<TerminalBackground>,
}

impl Workspace {
    fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        saved: settings::Settings,
        settings_error: Option<String>,
        layout_session: layout_state::Session,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        let (boomux_overview, boomux_error) = match terminal::discover_overview() {
            Ok(overview) => (overview, None),
            Err(error) => (BoomuxOverview::default(), Some(error)),
        };
        let boomux_shells = boomux_overview
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.shells.iter().cloned())
            .collect::<Vec<_>>();
        let workspace_order = boomux_overview
            .workspaces
            .iter()
            .map(|workspace| workspace.id.clone())
            .collect();

        let layout = Node::pane(1);
        let mut terminals = HashMap::new();
        terminals.insert(1, TerminalPane::default());

        let requested_shell_id = std::env::var("BOOMUX_DESKTOP_SHELL_ID").ok();
        let initial_shell = requested_shell_id
            .as_ref()
            .and_then(|requested_id| {
                boomux_shells
                    .iter()
                    .find(|shell| shell.id == *requested_id)
                    .cloned()
            })
            .or_else(|| {
                boomux_overview
                    .focused_shell_id
                    .as_ref()
                    .and_then(|focused_id| {
                        boomux_shells
                            .iter()
                            .find(|shell| shell.id == *focused_id)
                            .cloned()
                    })
            })
            .or_else(|| boomux_shells.first().cloned());
        let expanded_workspaces = initial_shell
            .as_ref()
            .map(|shell| HashSet::from([shell.workspace_id.clone()]))
            .unwrap_or_default();
        let sidebar_scroll_handle = ScrollHandle::new();
        let sidebar_scroll_anchor = ScrollAnchor::for_handle(sidebar_scroll_handle.clone());
        let sidebar_agent_scroll_handle = ScrollHandle::new();
        let sidebar_agent_scroll_anchor =
            ScrollAnchor::for_handle(sidebar_agent_scroll_handle.clone());
        let minimized_tab_scroll_handle = ScrollHandle::new();
        let help_scroll_handle = ScrollHandle::new();
        let mut workspace = Self {
            layout_document: layout_session.document,
            layout_writer: layout_session.writer,
            layout_save_task: None,
            layout_error: layout_session.error,
            layout_canvas: (1.0, 1.0),
            layout_generation: 0,
            layout_frozen: false,
            layout_closing: false,
            restore_pointer_guard: layout_persistence::PointerGuard::Inactive,
            layout_restoring: false,
            layout: Some(layout),
            floating: Vec::new(),
            git_panel: git_panel::Model::default(),
            pointer_drag: None,
            terminal_scrollbar_drag: None,
            terminal_selection_release: None,
            copied_pane: None,
            copied_cleanup: None,
            layout_animation: None,
            workspace_order_animation: None,
            floating_animation: None,
            minimizing_panes: Vec::new(),
            workspace_transition: None,
            animation_generation: 0,
            focused: 1,
            fullscreen: None,
            boomux_shells,
            previous_agent_states: boomux_overview
                .agents
                .iter()
                .map(|agent| (agent.id.clone(), agent.state))
                .collect(),
            boomux_overview,
            completed_agents: HashSet::new(),
            dismissing_agents: HashSet::new(),
            workspace_order,
            boomux_error,
            expanded_workspaces,
            navigation_region: NavigationRegion::Terminal,
            sidebar_focus_pointer: None,
            sidebar_item: None,
            sidebar_scroll_handle,
            sidebar_agent_scroll_handle,
            sidebar_agent_scroll_anchor,
            sidebar_scroll_anchor,
            minimized_tab_scroll_handle,
            sidebar_menu: None,
            sidebar_header_menu_open: false,
            project_menu_open: false,
            projects_loading: false,
            projects_result: None,
            project_folder_picker_pending: false,
            project_folder_picker_open: false,
            nodes_open: false,
            node_views: Vec::new(),
            nodes_error: None,
            node_forget_confirm: None,
            node_forget_busy: false,
            node_forget_error: None,
            selected_node: None,
            expanded_node: None,
            resource_dialog: None,
            sidebar_visible: saved.sidebar_visible,
            sidebar_preferred_width: saved.sidebar_width,
            sidebar_viewport_width: f32::from(window.viewport_size().width),
            sidebar_resizing: false,
            sidebar_drag_width: None,
            drawer_animation_from: None,
            drawer_animation_generation: 0,
            pane_headings_visible: saved.pane_headings_visible,
            pane_corner_style: saved.pane_corner_style,
            pane_gap: saved.pane_gap,
            focus_highlight_strength: saved.focus_highlight_strength,
            motion_speed: saved.motion_speed,
            layout_overlay_visible: saved.layout_overlay_visible,
            copy_on_select: saved.copy_on_select,
            workspace_pane_mode: saved.workspace_pane_mode,
            pane_layout_mode: saved.pane_layout_mode,
            minimized_shells: HashSet::new(),
            confirm_destructive_actions: saved.confirm_destructive_actions,
            theme: AppTheme::default(),
            theme_watcher: None,
            theme_load_generation: 0,
            theme_error: None,
            update_check: updates::Check::default(),
            updates_checking: false,
            update_busy: false,
            prepared_update: None,
            onboarding_complete: saved.onboarding_complete,
            updates_status: None,
            update_task: None,
            dismissed_desktop_update: saved.dismissed_desktop_update,
            dismissed_boomux_update: saved.dismissed_boomux_update,
            settings_open: false,
            settings_writer: None,
            settings_error,
            settings_restart_pending: saved.settings_restart_pending,
            settings_restart_confirm: false,
            boomux_settings_snapshot: None,
            boomux_settings_busy: false,
            boomux_settings_message: None,
            boomux_setting_input: None,
            help_open: false,
            help_scroll_handle,
            layout_mode: false,
            layout_mode_entered_at: None,
            layout_leader_pressed_at: None,
            layout_leader_entered: false,
            layout_leader_release_task: None,
            layout_suppressed_keys: HashSet::new(),
            layout_badge_generation: 0,
            layout_badge_exiting: false,
            layout_badge_cleanup: None,
            terminal_pressed_keys: HashMap::new(),
            terminals,
            next_id: 2,
            focus_handle,
        };
        workspace.initialize_layout(window, cx);
        if workspace.layout_document.active.is_empty()
            && let Some(shell) = initial_shell
        {
            let workspace_id = shell.workspace_id.clone();
            let shell_id = shell.id.clone();
            workspace.open_workspace(&workspace_id, Some(&shell_id), window, cx);
        }
        if workspace.settings_error.is_none()
            && let Some(path) = settings::path()
        {
            let (writer, updates, completion) = settings::writer(path);
            workspace.settings_writer = Some(writer);
            cx.on_app_quit(move |this, _| {
                if let Some(writer) = this.settings_writer.take() {
                    writer.close();
                }
                let completion = completion.clone();
                async move {
                    let _ = completion.recv().await;
                }
            })
            .detach();
            cx.spawn(async move |this, cx| {
                while let Ok(result) = updates.recv().await {
                    if this
                        .update(cx, |this, cx| {
                            this.settings_error = result.err();
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
        }
        if let Some(requested) = requested_shell_id.as_deref() {
            workspace.activate_sidebar_shell(requested, window, cx);
        }
        workspace.watch_updates(cx);
        cx.observe_window_activation(window, |this, window, cx| {
            if !window.is_window_active() {
                this.terminal_selection_release = None;
                this.layout_leader_release_task = None;
                if this.layout_leader_pressed_at.take().is_some() && this.layout_leader_entered {
                    this.leave_layout_mode(cx);
                }
                this.layout_leader_entered = false;
                this.layout_suppressed_keys.clear();
            }
            cx.notify();
        })
        .detach();
        workspace.watch_omarchy_theme(cx);
        workspace.watch_boomux_overview(window, cx);
        if saved.sidebar_git_tab {
            workspace.select_git_tab(true, cx);
        }
        workspace
    }

    fn save_settings(&self) {
        if let Some(writer) = &self.settings_writer {
            let _ = writer.force_send(settings::Settings {
                onboarding_complete: self.onboarding_complete,
                sidebar_visible: self.sidebar_visible,
                sidebar_git_tab: self.git_panel.open,
                sidebar_width: self.sidebar_preferred_width,
                pane_headings_visible: self.pane_headings_visible,
                pane_corner_style: self.pane_corner_style,
                pane_gap: self.pane_gap,
                focus_highlight_strength: self.focus_highlight_strength,
                motion_speed: self.motion_speed,
                layout_overlay_visible: self.layout_overlay_visible,
                copy_on_select: self.copy_on_select,
                workspace_pane_mode: self.workspace_pane_mode,
                pane_layout_mode: self.pane_layout_mode,
                confirm_destructive_actions: self.confirm_destructive_actions,
                settings_restart_pending: self.settings_restart_pending,
                dismissed_desktop_update: self.dismissed_desktop_update.clone(),
                dismissed_boomux_update: self.dismissed_boomux_update.clone(),
            });
        }
    }

    fn watch_updates(&mut self, cx: &mut Context<Self>) {
        self.update_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_secs(10))
                .await;
            loop {
                if this.update(cx, |this, cx| this.check_updates(cx)).is_err() {
                    break;
                }
                cx.background_executor()
                    .timer(Duration::from_secs(6 * 60 * 60))
                    .await;
            }
        }));
    }

    fn check_updates(&mut self, cx: &mut Context<Self>) {
        if self.updates_checking || self.update_busy {
            return;
        }
        self.updates_checking = true;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async { updates::check() })
                .await;
            this.update(cx, |this, cx| {
                this.updates_checking = false;
                if this.updates_status.is_some() && !this.update_busy {
                    this.updates_status = if result.unavailable {
                        Some("Some update checks were unavailable. Try again later.".into())
                    } else if result.desktop.is_none()
                        && result.boomux.is_none()
                        && result.prepared.is_none()
                    {
                        Some("No newer releases found.".into())
                    } else {
                        None
                    };
                }
                if !this.update_busy {
                    this.prepared_update = result.prepared.clone();
                }
                this.update_check = result;
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn download_update(&mut self, version: String, cx: &mut Context<Self>) {
        if self.update_busy {
            return;
        }
        self.update_busy = true;
        self.updates_status = Some(format!("Downloading and verifying {version}…"));
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    bundle_update::Installation::discover()
                        .ok_or_else(|| {
                            "Reopen an official Desktop installation to update".to_string()
                        })?
                        .prepare(&version)
                })
                .await;
            this.update(cx, |this, cx| {
                this.update_busy = false;
                match result {
                    Ok(prepared) => {
                        this.prepared_update = Some(prepared);
                        this.updates_status = None;
                    }
                    Err(error) => this.updates_status = Some(error),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn restart_for_update(&mut self, cx: &mut Context<Self>) {
        if self.update_busy {
            return;
        }
        let Some(prepared) = self.prepared_update.clone() else {
            return;
        };
        self.capture_arrangement();
        if self.layout_error.as_deref() == Some("Saved layout limit reached") {
            self.updates_status =
                Some("Reduce the saved layout before restarting; its limit was reached.".into());
            cx.notify();
            return;
        }
        self.layout_save_task.take();
        self.layout_frozen = true;
        let layout_flush = self
            .layout_writer
            .as_ref()
            .map(|writer| layout_state::submit(writer, self.layout_document.clone()));
        self.update_busy = true;
        self.updates_status = Some("Restarting Boomux and reopening Desktop…".into());
        cx.spawn(async move |this, cx| {
            let saved = match layout_flush {
                Some(flush) => flush
                    .recv()
                    .await
                    .unwrap_or_else(|_| Err("Layout save was interrupted".into())),
                None => Ok(()),
            };
            let result = match saved {
                Ok(()) => cx.background_spawn(async move { prepared.restart() }).await,
                Err(error) => Err(format!("Could not save layout before restart: {error}")),
            };
            this.update(cx, |this, cx| {
                this.update_busy = false;
                match result {
                    Ok(()) => cx.quit(),
                    Err(error) => {
                        this.layout_frozen = false;
                        this.updates_status = Some(error);
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn update_notices(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let mut rows = Vec::new();
        if let Some(status) = &self.updates_status {
            rows.push(
                div()
                    .mb_3()
                    .text_xs()
                    .text_color(rgb(0xa6adc8))
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().min_w_0().child(status.clone()))
                    .when(!self.updates_checking && !self.update_busy, |row| {
                        row.child(
                            Self::settings_option("dismiss-update-status", "Dismiss", false)
                                .flex_none()
                                .h(px(24.0))
                                .px_2()
                                .text_xs()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if !this.updates_checking && !this.update_busy {
                                        this.updates_status = None;
                                        cx.notify();
                                    }
                                })),
                        )
                    })
                    .into_any_element(),
            );
        }
        if let Some(prepared) = &self.prepared_update
            && prepared.version != self.dismissed_desktop_update
        {
            let version = prepared.version.clone();
            rows.push(div().mb_3().p_3().border_1().border_color(rgb(0x45475a))
                .flex().flex_col().gap_2()
                .child(format!("Boomux {} is ready", prepared.version))
                .child(div().text_xs().text_color(rgb(0xa6adc8))
                    .child("Restart the app and its background service to finish updating. Running terminals and commands will be preserved."))
                .child(Self::settings_option("restart-update", if self.update_busy { "Restarting…" } else { "Restart now" }, true)
                    .on_click(cx.listener(|this, _, _, cx| this.restart_for_update(cx))))
                .child(Self::settings_option("later-update", "Later", false)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.update_busy {
                            this.dismissed_desktop_update = version.clone();
                            this.dismissed_boomux_update = version.clone();
                            this.save_settings();
                            cx.notify();
                        }
                    })))
                .into_any_element());
        }
        if let Some(notice) = self.update_check.release_notice(
            &self.dismissed_desktop_update,
            &self.dismissed_boomux_update,
        ) && self.prepared_update.is_none()
        {
            let download_version = notice.latest.clone();
            let version = notice.latest.clone();
            let url = notice.url.clone();
            rows.push(
                div()
                    .mb_3()
                    .p_3()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(0x45475a))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0xcdd6f4))
                            .child("Boomux update available"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(0xa6adc8))
                            .child(self.update_check.version_summary(notice)),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                Self::settings_option("view-update", "View release", false)
                                    .on_click(cx.listener(move |_, _, _, cx| cx.open_url(&url))),
                            )
                            .when(
                                self.update_check.installable
                                    && self
                                        .update_check
                                        .desktop
                                        .as_ref()
                                        .is_some_and(|desktop| desktop.latest == notice.latest),
                                |row| {
                                    row.child(
                                        Self::settings_option(
                                            "download-update",
                                            if self.update_busy {
                                                "Updating…"
                                            } else {
                                                "Update"
                                            },
                                            true,
                                        )
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                this.download_update(download_version.clone(), cx)
                                            }),
                                        ),
                                    )
                                },
                            )
                            .child(
                                Self::settings_option("dismiss-update", "Dismiss", false).on_click(
                                    cx.listener(move |this, _, _, cx| {
                                        this.dismissed_desktop_update = version.clone();
                                        this.dismissed_boomux_update = version.clone();
                                        this.save_settings();
                                        cx.notify();
                                    }),
                                ),
                            ),
                    )
                    .into_any_element(),
            );
        }
        rows
    }

    fn watch_omarchy_theme(&mut self, cx: &mut Context<Self>) {
        let Some(state_directory) = theme::omarchy_state_directory() else {
            return;
        };
        let colors_path = theme::omarchy_colors_path(&state_directory);
        match ThemeWatcher::new(&state_directory) {
            Ok(watcher) => {
                let updates = watcher.updates.clone();
                self.theme_watcher = Some(watcher);
                let watched_colors_path = colors_path.clone();
                cx.spawn(async move |this, cx| {
                    while updates.recv().await.is_ok() {
                        cx.background_executor()
                            .timer(Duration::from_millis(100))
                            .await;
                        while updates.try_recv().is_ok() {}
                        this.update(cx, |this, cx| {
                            this.reload_omarchy_theme(watched_colors_path.clone(), cx);
                        })
                        .ok();
                    }
                })
                .detach();
            }
            Err(error) => self.theme_error = Some(error),
        }
        self.reload_omarchy_theme(colors_path, cx);
    }

    fn reload_omarchy_theme(&mut self, colors_path: std::path::PathBuf, cx: &mut Context<Self>) {
        self.theme_load_generation = self.theme_load_generation.wrapping_add(1);
        let generation = self.theme_load_generation;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { AppTheme::load_omarchy(&colors_path) })
                .await;
            this.update(cx, |this, cx| {
                if this.theme_load_generation != generation {
                    return;
                }
                match result {
                    Ok(theme) => {
                        this.theme = theme;
                        theme::install(theme);
                        this.theme_error = this.terminals.values().find_map(|pane| {
                            pane.session
                                .as_ref()
                                .and_then(|session| session.set_theme(theme.terminal).err())
                        });
                    }
                    Err(error) => this.theme_error = Some(error),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn focus_direction(
        &mut self,
        direction: Direction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.navigation_region == NavigationRegion::Sidebar {
            if direction == Direction::Right {
                self.leave_sidebar(cx);
            }
            return;
        }

        if let Some(id) = self
            .layout
            .as_ref()
            .and_then(|layout| layout.neighbor(self.focused, direction))
        {
            self.focused = id;
            self.layout_changed(cx);
            cx.notify();
        } else if direction == Direction::Left {
            self.enter_sidebar(window, cx);
        }
    }

    fn raise_floating_pane(&mut self, pane_id: usize) {
        if let Some(index) = self.floating.iter().position(|pane| pane.id == pane_id)
            && index + 1 != self.floating.len()
        {
            let pane = self.floating.remove(index);
            self.floating.push(pane);
        }
    }

    fn cycle_pane(&mut self, backwards: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.navigation_region == NavigationRegion::Sidebar {
            self.leave_sidebar(cx);
        }
        let pane_ids = ordered_pane_ids(self.layout.as_ref(), &self.floating);
        let Some(next) = cycled_pane_id(&pane_ids, self.focused, backwards) else {
            return;
        };
        self.focus_terminal_pane(next, window, cx);
    }

    fn cycle_workspace(&mut self, backwards: bool, window: &mut Window, cx: &mut Context<Self>) {
        let current_workspace_id = self
            .terminals
            .get(&self.focused)
            .and_then(|pane| pane.shell.as_ref())
            .map(|shell| shell.workspace_id.as_str());
        let Some(workspace_id) =
            cycled_workspace_id(&self.workspace_order, current_workspace_id, backwards)
                .map(str::to_owned)
        else {
            return;
        };
        if current_workspace_id == Some(workspace_id.as_str()) {
            return;
        }
        let preferred_shell_id = self
            .boomux_overview
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .and_then(|workspace| {
                workspace
                    .shells
                    .iter()
                    .find(|shell| !self.minimized_shells.contains(&shell.id))
            })
            .map(|shell| shell.id.clone());
        self.open_workspace(&workspace_id, preferred_shell_id.as_deref(), window, cx);
    }

    fn focus_terminal_pane(&mut self, pane_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.git_panel.search_focused = false;
        if !self.terminals.contains_key(&pane_id) {
            return;
        }
        self.focused = pane_id;
        self.raise_floating_pane(pane_id);
        self.navigation_region = NavigationRegion::Terminal;
        self.sidebar_focus_pointer = None;
        window.focus(&self.focus_handle, cx);
        if let Some(terminal) = self
            .terminals
            .get(&pane_id)
            .and_then(|pane| pane.session.as_ref())
        {
            terminal.focus();
        }
        self.layout_changed(cx);
        cx.notify();
    }

    fn preferred_sidebar_item(&self, visible: &[SidebarItem]) -> Option<SidebarItem> {
        let pane = self.terminals.get(&self.focused);
        let shell_id = pane
            .and_then(|pane| pane.session.as_ref())
            .map(|terminal| terminal.shell_id.as_str());
        if let Some(shell) = shell_id
            && let Some(item) = visible.iter().find(
                |item| matches!(item, SidebarItem::Shell { shell_id, .. } if shell_id == shell),
            )
        {
            return Some(item.clone());
        }

        let workspace_id = pane
            .and_then(|pane| pane.shell.as_ref())
            .map(|shell| shell.workspace_id.as_str());
        workspace_id.and_then(|workspace| {
            visible
                .iter()
                .find(|item| matches!(item, SidebarItem::Workspace(id) if id == workspace))
                .cloned()
        })
    }

    fn sidebar_navigation_items(&self) -> Vec<SidebarItem> {
        visible_sidebar_items(&self.boomux_overview, &self.expanded_workspaces)
            .into_iter()
            .filter(|item| sidebar_item_visible_in_layout(self.pane_layout_mode, item))
            .filter(|item| !self.git_panel.open || !matches!(item, SidebarItem::Agent { .. }))
            .collect()
    }

    fn reconcile_sidebar_item(&mut self) {
        let visible = self.sidebar_navigation_items();
        let preferred = self.preferred_sidebar_item(&visible);
        self.sidebar_item =
            reconciled_sidebar_item(self.sidebar_item.as_ref(), preferred.as_ref(), &visible);
    }

    fn set_boomux_overview(&mut self, mut overview: BoomuxOverview) {
        reconcile_workspace_order(&mut self.workspace_order, &mut overview);
        reconcile_completed_agents(
            &mut self.previous_agent_states,
            &mut self.completed_agents,
            &overview.agents,
        );
        self.boomux_shells = overview
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.shells.iter().cloned())
            .collect();
        self.boomux_overview = overview;
    }

    fn dismiss_agent_notification(
        &mut self,
        agent_id: String,
        attention_revision: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        self.completed_agents.remove(&agent_id);
        let Some(revision) = attention_revision else {
            cx.notify();
            return;
        };
        if !self.dismissing_agents.insert(agent_id.clone()) {
            return;
        }
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn({
                    let agent_id = agent_id.clone();
                    async move {
                        terminal::acknowledge_agent_attention(&agent_id, revision)?;
                        terminal::discover_overview()
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                this.dismissing_agents.remove(&agent_id);
                match result {
                    Ok(overview) => {
                        this.set_boomux_overview(overview);
                        this.boomux_error = None;
                        if this.navigation_region == NavigationRegion::Sidebar {
                            this.reconcile_sidebar_item();
                        }
                    }
                    Err(error) => this.boomux_error = Some(error),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn reorder_workspace_relative(
        &mut self,
        source: &str,
        target: &str,
        after: bool,
        cx: &mut Context<Self>,
    ) {
        let from = sidebar_workspace_offsets(
            &self.boomux_overview,
            &self.expanded_workspaces,
            self.pane_layout_mode,
        );
        if reorder_workspace(&mut self.workspace_order, source, target, after) {
            reconcile_workspace_order(&mut self.workspace_order, &mut self.boomux_overview);
            if let Some(duration) = self.motion_speed.duration() {
                self.animation_generation = self.animation_generation.wrapping_add(1);
                let generation = self.animation_generation;
                self.workspace_order_animation = Some(WorkspaceOrderAnimation { from, generation });
                cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(duration).await;
                    this.update(cx, |this, cx| {
                        if this
                            .workspace_order_animation
                            .as_ref()
                            .is_some_and(|animation| animation.generation == generation)
                        {
                            this.workspace_order_animation = None;
                            cx.notify();
                        }
                    })
                    .ok();
                })
                .detach();
            } else {
                self.workspace_order_animation = None;
            }
            self.layout_changed(cx);
            cx.notify();
        }
    }

    fn drag_workspace(
        &mut self,
        event: &DragMoveEvent<WorkspaceRowDrag>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let source = event.drag(cx).workspace_id.clone();
        let pointer_y = f32::from(event.event.position.y - event.bounds.top());
        if let Some((target, after)) = sidebar_workspace_drop_target(
            &self.boomux_overview,
            &self.expanded_workspaces,
            self.pane_layout_mode,
            pointer_y,
        ) {
            self.reorder_workspace_relative(&source, &target, after, cx);
        }
        cx.stop_propagation();
    }

    fn move_selected_workspace(&mut self, offset: isize, cx: &mut Context<Self>) -> bool {
        let Some(SidebarItem::Workspace(workspace_id)) = self.sidebar_item.clone() else {
            return false;
        };
        let Some(index) = self
            .workspace_order
            .iter()
            .position(|id| id == &workspace_id)
        else {
            return false;
        };
        let target = index.saturating_add_signed(offset);
        if target >= self.workspace_order.len() || target == index {
            return true;
        }
        let target_id = self.workspace_order[target].clone();
        self.reorder_workspace_relative(&workspace_id, &target_id, offset.is_positive(), cx);
        true
    }

    fn reveal_sidebar_item(&self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.sidebar_item, Some(SidebarItem::Agent { .. })) {
            self.sidebar_agent_scroll_anchor.scroll_to(window, cx);
        } else {
            self.sidebar_scroll_anchor.scroll_to(window, cx);
        }
    }

    fn enter_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.sidebar_visible {
            self.drawer_animation_from = Some(0.0);
            self.drawer_animation_generation = self.drawer_animation_generation.wrapping_add(1);
            self.sidebar_visible = true;
            self.save_settings();
            let panel_size = self.panel_size(window);
            for pane in &mut self.floating {
                *pane = clamp_floating_to_panel(pane.clone(), panel_size);
            }
        }
        self.navigation_region = NavigationRegion::Sidebar;
        let pointer = window.mouse_position();
        self.sidebar_focus_pointer = Some((f32::from(pointer.x), f32::from(pointer.y)));
        self.reconcile_sidebar_item();
        window.focus(&self.focus_handle, cx);
        self.reveal_sidebar_item(window, cx);
        cx.notify();
    }

    fn leave_sidebar(&mut self, cx: &mut Context<Self>) {
        self.navigation_region = NavigationRegion::Terminal;
        self.sidebar_focus_pointer = None;
        if let Some(terminal) = self
            .terminals
            .get(&self.focused)
            .and_then(|pane| pane.session.as_ref())
        {
            terminal.focus();
        }
        cx.notify();
    }

    fn toggle_sidebar_focus(
        &mut self,
        _: &ToggleSidebarFocus,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.navigation_region == NavigationRegion::Sidebar {
            self.leave_sidebar(cx);
        } else {
            self.enter_sidebar(window, cx);
        }
    }

    fn toggle_layout_mode(
        &mut self,
        _: &ToggleLayoutMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // OS key repeat must not turn one held leader into repeated toggles.
        if self.layout_leader_pressed_at.is_some() {
            self.layout_leader_release_task = None;
            cx.stop_propagation();
            return;
        }
        self.layout_leader_pressed_at = Some(Instant::now());
        self.layout_leader_entered = !self.layout_mode;
        if self.layout_mode {
            let pass_through = self
                .layout_mode_entered_at
                .is_some_and(|entered| layout_leader_passes_through(entered.elapsed()));
            self.leave_layout_mode(cx);
            if pass_through {
                let leader = gpui::Keystroke {
                    key: "space".into(),
                    key_char: None,
                    modifiers: gpui::Modifiers {
                        control: true,
                        ..Default::default()
                    },
                };
                if let Some(terminal) = self
                    .terminals
                    .get(&self.focused)
                    .and_then(|pane| pane.session.as_ref())
                {
                    terminal.send_key(&leader, libghostty_vt::key::Action::Press);
                    terminal.send_key(&leader, libghostty_vt::key::Action::Release);
                }
            }
        } else {
            self.layout_badge_cleanup = None;
            self.layout_badge_exiting = false;
            self.layout_badge_generation = self.layout_badge_generation.wrapping_add(1);
            self.layout_mode = true;
            self.layout_mode_entered_at = Some(Instant::now());
            window.focus(&self.focus_handle, cx);
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn release_layout_leader(&mut self, cx: &mut Context<Self>) {
        let elapsed = self
            .layout_leader_pressed_at
            .map(|pressed| pressed.elapsed());
        self.release_layout_leader_at(elapsed, cx);
    }

    fn release_layout_leader_at(&mut self, elapsed: Option<Duration>, cx: &mut Context<Self>) {
        if elapsed
            .is_some_and(|elapsed| layout_leader_release_exits(self.layout_leader_entered, elapsed))
            && self.layout_mode
        {
            self.leave_layout_mode(cx);
        }
        self.layout_leader_entered = false;
    }

    fn leave_layout_mode(&mut self, cx: &mut Context<Self>) {
        self.layout_mode = false;
        self.layout_mode_entered_at = None;
        self.layout_badge_cleanup = None;
        self.layout_badge_exiting = false;
        if let Some(duration) = self.motion_speed.duration() {
            self.layout_badge_exiting = true;
            // One pane-independent task, canceled on re-entry or when the app closes.
            self.layout_badge_cleanup = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(duration).await;
                this.update(cx, |this, cx| {
                    this.layout_badge_exiting = false;
                    cx.notify();
                })
                .ok();
            }));
        }
    }

    fn exit_layout_mode(
        &mut self,
        _: &ExitLayoutMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        // Keep the Layout context active through this key's raw event so the
        // Escape used to leave the mode is not also forwarded to the PTY.
        cx.defer_in(window, |this, _, cx| {
            this.leave_layout_mode(cx);
            cx.notify();
        });
    }

    fn toggle_sidebar_drawer(
        &mut self,
        _: &ToggleSidebarDrawer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previous_width = self.sidebar_width();
        self.project_menu_open = false;
        self.sidebar_visible = !self.sidebar_visible;
        self.nodes_open = false;
        self.save_settings();
        self.drawer_animation_generation = self.drawer_animation_generation.wrapping_add(1);
        self.drawer_animation_from = Some(previous_width);
        self.sidebar_menu = None;
        self.sidebar_header_menu_open = false;
        let panel_size = self.panel_size(window);
        for pane in &mut self.floating {
            *pane = clamp_floating_to_panel(pane.clone(), panel_size);
        }
        if !self.sidebar_visible && self.navigation_region == NavigationRegion::Sidebar {
            self.leave_sidebar(cx);
        } else {
            cx.notify();
        }
    }

    fn toggle_settings(&mut self, cx: &mut Context<Self>) {
        self.project_menu_open = false;
        self.git_panel.search_focused = false;
        self.sidebar_header_menu_open = false;
        if self.settings_open {
            self.close_settings(cx);
            return;
        }
        self.settings_open = true;
        if self.settings_open {
            if self.boomux_settings_snapshot.is_none() {
                self.load_boomux_settings(cx);
            }
            self.help_open = false;
            self.sidebar_menu = None;
        }
        cx.notify();
    }

    fn close_settings(&mut self, cx: &mut Context<Self>) {
        self.project_folder_picker_pending = false;
        if !self.settings_open {
            return;
        }
        self.settings_open = false;
        self.boomux_setting_input = None;
        self.settings_restart_confirm = self.settings_restart_pending;
        cx.notify();
    }

    fn set_pane_layout_mode(
        &mut self,
        mode: PaneLayoutMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.pane_layout_mode == mode {
            return;
        }
        self.capture_arrangement();
        self.pane_layout_mode = mode;
        if mode == PaneLayoutMode::Tabbed {
            self.workspace_pane_mode = WorkspacePaneMode::Workspace;
            let workspace_id = self
                .terminals
                .get(&self.focused)
                .and_then(|pane| pane.shell.as_ref())
                .map(|shell| shell.workspace_id.clone());
            if let Some(workspace_id) = workspace_id {
                self.open_workspace(&workspace_id, None, window, cx);
            }
        }
        self.save_settings();
        self.reconcile_sidebar_item();
        self.layout_changed(cx);
        cx.notify();
    }

    fn toggle_help(&mut self, _: &ToggleHelp, _: &mut Window, cx: &mut Context<Self>) {
        if !self.help_open && self.resource_dialog.is_some() {
            return;
        }
        self.sidebar_header_menu_open = false;
        self.help_open = !self.help_open;
        if self.help_open {
            self.close_settings(cx);
            self.sidebar_menu = None;
            self.help_scroll_handle.set_offset(point(px(0.0), px(0.0)));
        }
        cx.notify();
    }

    fn help_key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let current = self.help_scroll_handle.offset();
        let maximum = self.help_scroll_handle.max_offset();
        let next_y = match event.keystroke.key.as_str() {
            "escape" => {
                self.help_open = false;
                current.y
            }
            "up" | "k" => (current.y + px(48.0)).min(px(0.0)),
            "down" | "j" => (current.y - px(48.0)).max(-maximum.y),
            "pageup" => (current.y + px(360.0)).min(px(0.0)),
            "pagedown" | "space" => (current.y - px(360.0)).max(-maximum.y),
            "home" => px(0.0),
            "end" => -maximum.y,
            _ => current.y,
        };
        if next_y != current.y {
            self.help_scroll_handle.set_offset(point(current.x, next_y));
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn sidebar_resource(&self, item: &SidebarItem) -> Option<SidebarResource> {
        match item {
            SidebarItem::Workspace(id) => self
                .boomux_overview
                .workspaces
                .iter()
                .find(|workspace| workspace.id == *id)
                .map(|workspace| SidebarResource::Workspace {
                    id: workspace.id.clone(),
                    name: workspace.name.clone(),
                }),
            SidebarItem::Shell { shell_id, .. } | SidebarItem::Agent { shell_id, .. } => self
                .boomux_shells
                .iter()
                .find(|shell| shell.id == *shell_id)
                .map(|shell| SidebarResource::Shell {
                    id: shell.id.clone(),
                    workspace_id: shell.workspace_id.clone(),
                    name: shell.name.clone(),
                }),
        }
    }

    fn focused_shell_resource(&self) -> Option<SidebarResource> {
        self.terminals
            .get(&self.focused)
            .and_then(|pane| pane.shell.as_ref())
            .map(|shell| SidebarResource::Shell {
                id: shell.id.clone(),
                workspace_id: shell.workspace_id.clone(),
                name: shell.name.clone(),
            })
    }

    fn keyboard_resource(&self) -> Option<SidebarResource> {
        if self.navigation_region == NavigationRegion::Sidebar {
            self.sidebar_item
                .as_ref()
                .and_then(|item| self.sidebar_resource(item))
        } else {
            self.focused_shell_resource()
        }
    }

    fn open_sidebar_menu(
        &mut self,
        target: SidebarResource,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let menu_height = if matches!(target, SidebarResource::Workspace { .. }) {
            146.0
        } else {
            78.0
        };
        let maximum = (f32::from(window.viewport_size().height) - menu_height - 8.0).max(8.0);
        self.sidebar_header_menu_open = false;
        self.sidebar_menu = Some(SidebarMenu {
            target,
            top: f32::from(event.position().y).clamp(8.0, maximum),
        });
        cx.stop_propagation();
        cx.notify();
    }

    fn open_resource_dialog(&mut self, kind: ResourceDialogKind, target: SidebarResource) {
        let value = target.name().to_string();
        self.sidebar_menu = None;
        self.resource_dialog = Some(ResourceDialog {
            kind,
            target,
            value,
            busy: false,
            error: None,
        });
    }

    fn request_remove_resource(
        &mut self,
        target: SidebarResource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_resource_dialog(ResourceDialogKind::Remove, target);
        if self.confirm_destructive_actions {
            cx.notify();
        } else {
            self.submit_resource_dialog(window, cx);
            self.resource_dialog = None;
            cx.notify();
        }
    }

    fn rename_resource(&mut self, _: &RenameResource, _: &mut Window, cx: &mut Context<Self>) {
        if self.resource_dialog.is_some() {
            return;
        }
        if let Some(target) = self.keyboard_resource() {
            self.open_resource_dialog(ResourceDialogKind::Rename, target);
            cx.notify();
        }
    }

    fn remove_shell(&mut self, _: &RemoveShell, window: &mut Window, cx: &mut Context<Self>) {
        if self.resource_dialog.is_some() {
            return;
        }
        if let Some(target @ SidebarResource::Shell { .. }) = self.keyboard_resource() {
            self.request_remove_resource(target, window, cx);
        }
    }

    fn remove_resource_panes(&mut self, target: &SidebarResource, window: &mut Window) {
        let pane_ids = self
            .terminals
            .iter()
            .filter_map(|(id, pane)| {
                let shell = pane.shell.as_ref()?;
                let matches = match target {
                    SidebarResource::Workspace { id, .. } => shell.workspace_id == *id,
                    SidebarResource::Shell { id, .. } => shell.id == *id,
                };
                matches.then_some(*id)
            })
            .collect::<Vec<_>>();
        for id in &pane_ids {
            if let Some(pane) = self.terminals.remove(id) {
                for image in pane.render_images.into_values() {
                    let _ = window.drop_image(image);
                }
            }
            self.floating.retain(|pane| pane.id != *id);
            self.layout = self.layout.take().and_then(|layout| layout.remove(*id));
            if self.fullscreen == Some(*id) {
                self.fullscreen = None;
            }
        }
        self.minimizing_panes
            .retain(|animation| !pane_ids.contains(&animation.pane_id));
        if self
            .pointer_drag
            .as_ref()
            .is_some_and(|drag| pane_ids.contains(&drag.pane_id))
        {
            self.pointer_drag = None;
        }
        self.focus_after_removal();
    }

    fn resource_dialog_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(dialog) = self.resource_dialog.as_mut() else {
            return;
        };
        let key = event.keystroke.key.as_str();
        if key == "escape" && !dialog.busy {
            self.resource_dialog = None;
        } else if key == "enter" {
            self.submit_resource_dialog(window, cx);
        } else if dialog.kind == ResourceDialogKind::Rename && !dialog.busy {
            if key == "backspace" {
                dialog.value.pop();
                dialog.error = None;
            } else if !event.keystroke.modifiers.control
                && !event.keystroke.modifiers.platform
                && !event.keystroke.modifiers.function
                && let Some(text) = event.keystroke.key_char.as_deref()
            {
                append_resource_name(&mut dialog.value, text);
                dialog.error = None;
            }
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn submit_resource_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.resource_dialog.as_mut() else {
            return;
        };
        if dialog.busy {
            return;
        }
        if dialog.kind == ResourceDialogKind::Rename && dialog.value.trim().is_empty() {
            dialog.error = Some("Name cannot be empty".into());
            cx.notify();
            return;
        }
        dialog.busy = true;
        dialog.error = None;
        let kind = dialog.kind;
        let target = dialog.target.clone();
        let value = dialog.value.trim().to_string();
        if kind == ResourceDialogKind::Remove {
            self.remove_resource_panes(&target, window);
        }
        cx.notify();

        cx.spawn(async move |this, cx| {
            let operation_target = target.clone();
            let operation_value = value.clone();
            let result = cx
                .background_spawn(async move {
                    match (&operation_target, kind) {
                        (SidebarResource::Workspace { id, .. }, ResourceDialogKind::Rename) => {
                            terminal::rename_workspace(id, &operation_value)?;
                        }
                        (SidebarResource::Shell { id, .. }, ResourceDialogKind::Rename) => {
                            terminal::rename_shell(id, &operation_value)?;
                        }
                        (SidebarResource::Workspace { id, .. }, ResourceDialogKind::Remove) => {
                            terminal::remove_workspace(id)?;
                        }
                        (SidebarResource::Shell { id, .. }, ResourceDialogKind::Remove) => {
                            terminal::close_shell(id)?;
                        }
                    }
                    terminal::discover_overview()
                })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(overview) => {
                        if kind == ResourceDialogKind::Rename
                            && let SidebarResource::Shell { id, .. } = &target
                        {
                            for pane in this.terminals.values_mut() {
                                if let Some(shell) =
                                    pane.shell.as_mut().filter(|shell| shell.id == *id)
                                {
                                    shell.name = value.clone();
                                }
                                if let Some(session) = pane
                                    .session
                                    .as_mut()
                                    .filter(|session| session.shell_id == *id)
                                {
                                    session.shell_name = value.clone();
                                }
                            }
                        }
                        this.set_boomux_overview(overview);
                        this.resource_dialog = None;
                        this.reconcile_sidebar_item();
                    }
                    Err(error) => {
                        if let Some(dialog) = this.resource_dialog.as_mut() {
                            dialog.busy = false;
                            dialog.error = Some(error);
                        } else {
                            this.boomux_error = Some(error);
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn move_direction(&mut self, direction: Direction, window: &Window, cx: &mut Context<Self>) {
        if self.fullscreen.is_some() {
            return;
        }
        self.layout_animation = None;
        self.floating_animation = None;
        let panel_size = self.panel_size(window);
        if let Some(floating) = self
            .floating
            .iter_mut()
            .find(|pane| pane.id == self.focused)
        {
            match direction {
                Direction::Left => floating.x -= 24.0,
                Direction::Right => floating.x += 24.0,
                Direction::Up => floating.y -= 24.0,
                Direction::Down => floating.y += 24.0,
            }
            *floating = clamp_floating_to_panel(floating.clone(), panel_size);
        } else if let Some(previous_rects) = self
            .layout
            .as_mut()
            .and_then(|layout| swap_layout_direction(layout, self.focused, direction))
        {
            self.begin_layout_animation(previous_rects);
        }
        self.layout_changed(cx);
        cx.notify();
    }

    fn resize_direction(
        &mut self,
        direction: Direction,
        tiled_amount: f32,
        floating_amount: f32,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self.fullscreen.is_some() {
            return;
        }
        self.layout_animation = None;
        self.floating_animation = None;
        let panel_size = self.panel_size(window);
        if let Some(floating) = self
            .floating
            .iter_mut()
            .find(|pane| pane.id == self.focused)
        {
            *floating = resize_floating_in_direction(
                floating.clone(),
                direction,
                floating_amount,
                panel_size,
            );
        } else {
            if let Some(layout) = &mut self.layout {
                layout.resize(self.focused, direction, tiled_amount);
            }
        }
        self.layout_changed(cx);
        cx.notify();
    }

    fn transform_nearest_split(
        &mut self,
        transform: impl FnOnce(&mut Node, usize) -> bool,
        cx: &mut Context<Self>,
    ) {
        if self.fullscreen.is_some() {
            return;
        }
        let Some(layout) = &mut self.layout else {
            return;
        };
        let previous = layout.rects().into_iter().collect::<HashMap<_, _>>();
        if transform(layout, self.focused) {
            self.begin_layout_animation(previous);
            self.layout_changed(cx);
            cx.notify();
        }
    }

    fn align_floating(
        &mut self,
        alignment: FloatingAlignment,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self
            .floating
            .iter()
            .position(|pane| pane.id == self.focused)
        else {
            return;
        };
        let from = self.floating[index].clone();
        let target = align_floating_to_panel(
            from.clone(),
            alignment,
            self.panel_size(window),
            self.pane_gap,
        );
        self.floating[index] = target;
        self.raise_floating_pane(self.focused);
        if self.motion_speed.duration().is_some() {
            self.animation_generation = self.animation_generation.wrapping_add(1);
            self.floating_animation = Some(FloatingAnimation {
                pane_id: self.focused,
                from,
                generation: self.animation_generation,
            });
        }
        self.layout_changed(cx);
        cx.notify();
    }

    fn new_pane(&mut self, _: &NewPane, window: &mut Window, cx: &mut Context<Self>) {
        if self.navigation_region == NavigationRegion::Sidebar
            && let Some(workspace_id) = self
                .sidebar_item
                .as_ref()
                .and_then(|item| workspace_id_for_sidebar_item(item, &self.boomux_overview))
        {
            self.create_and_attach_workspace_terminal(workspace_id, window, cx);
            return;
        }
        self.navigation_region = NavigationRegion::Terminal;
        self.fullscreen = None;
        self.layout_animation = None;
        let anchor = self
            .terminals
            .get(&self.focused)
            .and_then(|pane| pane.shell.clone())
            .or_else(|| self.boomux_shells.first().cloned());
        let id = self.insert_pane();
        self.focused = id;
        if let Some(anchor) = anchor {
            self.create_and_attach_terminal(id, anchor, window, cx);
        } else if let Some(pane) = self.terminals.get_mut(&id) {
            pane.error = Some("No Boomux workspace is available for a new terminal".into());
            self.layout_changed(cx);
            cx.notify();
        }
    }

    fn insert_pane(&mut self) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        let axis = if id.is_multiple_of(2) {
            Axis::Horizontal
        } else {
            Axis::Vertical
        };
        if let Some(layout) = &mut self.layout {
            let target = if layout.contains(self.focused) {
                self.focused
            } else {
                layout.pane_ids()[0]
            };
            layout.split(target, id, axis);
        } else {
            self.layout = Some(Node::pane(id));
        }
        self.terminals.insert(id, TerminalPane::default());
        id
    }

    fn close_pane(&mut self, _: &ClosePane, window: &mut Window, cx: &mut Context<Self>) {
        self.minimize_pane(self.focused, window, cx);
    }

    fn minimize_pane(&mut self, pane_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .minimizing_panes
            .iter()
            .any(|animation| animation.pane_id == pane_id)
        {
            return;
        }
        if let Some(shell) = self.terminals.get(&pane_id).and_then(|p| {
            p.shell
                .as_ref()
                .map(|s| s.id.clone())
                .or_else(|| p.restored.as_ref().and_then(|r| r.shell.clone()))
        }) {
            self.minimized_shells.insert(shell);
        }
        let from = self.pane_bounds_in_panel(pane_id, window);
        let previous_rects = self
            .layout
            .as_ref()
            .map(|layout| layout.rects().into_iter().collect::<HashMap<_, _>>())
            .unwrap_or_default();
        self.layout_animation = None;
        self.floating_animation = None;
        if self.fullscreen == Some(pane_id) {
            self.fullscreen = None;
        }
        if let Some(index) = self.floating.iter().position(|pane| pane.id == pane_id) {
            self.floating.remove(index);
            self.focus_after_removal();
        } else if self
            .layout
            .as_ref()
            .is_some_and(|layout| layout.contains(pane_id))
        {
            self.layout = self.layout.take().and_then(|layout| layout.remove(pane_id));
            self.focus_after_removal();
        }
        if !previous_rects.is_empty() {
            self.begin_layout_animation(previous_rects);
        }

        // Restore tabs must be available as soon as their pane leaves the layout.
        // Keeping the pane alive for the minimize animation delays tab creation.
        let minimize_duration = self
            .motion_speed
            .duration()
            .filter(|_| self.pane_layout_mode != PaneLayoutMode::Tabbed);
        if let (Some(duration), Some(from)) = (minimize_duration, from) {
            self.animation_generation = self.animation_generation.wrapping_add(1);
            let generation = self.animation_generation;
            self.minimizing_panes.push(PaneMinimizeAnimation {
                pane_id,
                from,
                generation,
                duration,
            });
            let window_handle = window.window_handle();
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(duration).await;
                let _ = window_handle.update(cx, |_, window, cx| {
                    this.update(cx, |this, cx| {
                        this.finish_minimize_animation(pane_id, generation, window, cx);
                    })
                });
            })
            .detach();
        } else {
            self.finish_minimize_pane(pane_id, window);
        }
        if self.has_minimized_tabs() {
            let panel_size = self.panel_size(window);
            for pane in &mut self.floating {
                *pane = clamp_floating_to_panel(pane.clone(), panel_size);
            }
        }
        self.layout_changed(cx);
        cx.notify();
    }

    fn finish_minimize_animation(
        &mut self,
        pane_id: usize,
        generation: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.minimizing_panes.iter().position(|animation| {
            animation.pane_id == pane_id && animation.generation == generation
        }) else {
            return;
        };
        self.minimizing_panes.remove(index);
        self.finish_minimize_pane(pane_id, window);
        if self.has_minimized_tabs() {
            let panel_size = self.panel_size(window);
            for pane in &mut self.floating {
                *pane = clamp_floating_to_panel(pane.clone(), panel_size);
            }
        }
        self.layout_changed(cx);
        cx.notify();
    }

    fn finish_minimize_pane(&mut self, pane_id: usize, window: &mut Window) {
        if let Some(pane) = self.terminals.remove(&pane_id) {
            if let Some(shell) = &pane.shell {
                self.minimized_shells.insert(shell.id.clone());
            }
            for image in pane.render_images.into_values() {
                let _ = window.drop_image(image);
            }
        }
    }

    fn request_pane_shell_dialog(
        &mut self,
        pane_id: usize,
        kind: ResourceDialogKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = self
            .terminals
            .get(&pane_id)
            .and_then(|pane| pane.shell.as_ref())
            .map(|shell| SidebarResource::Shell {
                id: shell.id.clone(),
                workspace_id: shell.workspace_id.clone(),
                name: shell.name.clone(),
            });
        if let Some(target) = target {
            if kind == ResourceDialogKind::Remove {
                self.request_remove_resource(target, window, cx);
            } else {
                self.open_resource_dialog(kind, target);
                cx.notify();
            }
        }
    }

    fn scroll_minimized_tabs(&mut self, direction: i8, cx: &mut Context<Self>) {
        let current = self.minimized_tab_scroll_handle.offset();
        let maximum = self.minimized_tab_scroll_handle.max_offset();
        let next_x = if direction < 0 {
            (current.x + px(204.0)).min(px(0.0))
        } else {
            (current.x - px(204.0)).max(-maximum.x)
        };
        self.minimized_tab_scroll_handle
            .set_offset(point(next_x, current.y));
        cx.notify();
    }

    fn toggle_floating(&mut self, _: &ToggleFloating, window: &mut Window, cx: &mut Context<Self>) {
        self.layout_animation = None;
        self.floating_animation = None;
        if let Some(index) = self
            .floating
            .iter()
            .position(|pane| pane.id == self.focused)
        {
            let pane = self.floating.remove(index);
            let mut previous_rects = self
                .layout
                .as_ref()
                .map(|layout| layout.rects().into_iter().collect::<HashMap<_, _>>())
                .unwrap_or_default();
            let (panel_width, panel_height) = self.panel_size(window);
            let (inner_width, inner_height) =
                inset_panel_size((panel_width, panel_height), self.pane_gap);
            previous_rects.insert(
                pane.id,
                Rect {
                    x: ((pane.x - self.pane_gap) / inner_width).clamp(0.0, 1.0),
                    y: ((pane.y - self.pane_gap) / inner_height).clamp(0.0, 1.0),
                    width: (pane.width / inner_width).clamp(0.0, 1.0),
                    height: (pane.height / inner_height).clamp(0.0, 1.0),
                },
            );
            if let Some(layout) = &mut self.layout {
                let target = layout.pane_ids()[0];
                layout.split(target, pane.id, Axis::Horizontal);
            } else {
                self.layout = Some(Node::pane(pane.id));
            }
            self.begin_layout_animation(previous_rects);
        } else if self
            .layout
            .as_ref()
            .is_some_and(|layout| layout.contains(self.focused))
        {
            let id = self.focused;
            if let Some(from) = self.lift_tiled_pane(id, window) {
                let target = centered_floating_pane(id, self.panel_size(window), self.pane_gap);
                if let Some(pane) = self.floating.iter_mut().find(|pane| pane.id == id) {
                    *pane = target;
                }
                if self.motion_speed.duration().is_some() {
                    self.animation_generation = self.animation_generation.wrapping_add(1);
                    self.floating_animation = Some(FloatingAnimation {
                        pane_id: id,
                        from,
                        generation: self.animation_generation,
                    });
                }
            }
        }
        self.layout_changed(cx);
        cx.notify();
    }

    fn toggle_fullscreen(
        &mut self,
        _: &ToggleFullscreen,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.pointer_drag.is_some()
            || self.terminal_scrollbar_drag.is_some()
            || self.sidebar_resizing
            || self.git_panel.resizing
        {
            return;
        }

        if let Some(id) = self.fullscreen.take() {
            if let Some(layout) = self.layout.as_ref().filter(|layout| layout.contains(id)) {
                let from = workspace_layout_rects(layout, Some(id))
                    .into_iter()
                    .collect();
                self.floating_animation = None;
                self.begin_layout_animation(from);
                if let Some(animation) = &mut self.layout_animation {
                    animation.paint_last = Some(id);
                }
            } else if self.floating.iter().any(|pane| pane.id == id) {
                self.layout_animation = None;
                if self.motion_speed.duration().is_some() {
                    self.animation_generation = self.animation_generation.wrapping_add(1);
                    self.floating_animation = Some(FloatingAnimation {
                        pane_id: id,
                        from: workspace_maximized_pane(id, self.panel_size(window), self.pane_gap),
                        generation: self.animation_generation,
                    });
                } else {
                    self.floating_animation = None;
                }
            }
            self.layout_changed(cx);
            cx.notify();
            return;
        }

        let id = self.focused;
        if let Some(layout) = self.layout.as_ref().filter(|layout| layout.contains(id)) {
            let from = workspace_layout_rects(layout, None).into_iter().collect();
            self.fullscreen = Some(id);
            self.floating_animation = None;
            self.begin_layout_animation(from);
        } else if let Some(pane) = self.floating.iter().find(|pane| pane.id == id).cloned() {
            self.fullscreen = Some(id);
            self.layout_animation = None;
            if self.motion_speed.duration().is_some() {
                self.animation_generation = self.animation_generation.wrapping_add(1);
                self.floating_animation = Some(FloatingAnimation {
                    pane_id: id,
                    from: pane,
                    generation: self.animation_generation,
                });
            } else {
                self.floating_animation = None;
            }
        }
        self.layout_changed(cx);
        cx.notify();
    }

    fn focus_after_removal(&mut self) {
        if let Some(pane) = self.floating.last() {
            self.focused = pane.id;
        } else if let Some(layout) = &self.layout {
            self.focused = layout.pane_ids()[0];
        }
    }

    // The drawer clips this layout during collapse; text keeps its readable width.
    fn sidebar_content_width(&self) -> f32 {
        self.sidebar_preferred_width
            .min((self.sidebar_viewport_width - 240.0).max(0.0))
    }

    fn sidebar_width(&self) -> f32 {
        if self.sidebar_visible {
            self.sidebar_drag_width
                .unwrap_or(self.sidebar_preferred_width)
                .min((self.sidebar_viewport_width - 240.0).max(0.0))
        } else {
            0.0
        }
    }

    fn panel_size(&self, window: &Window) -> (f32, f32) {
        let viewport = window.viewport_size();
        let tab_bar_height = if self.has_minimized_tabs() {
            TAB_BAR_HEIGHT
        } else {
            0.0
        };
        (
            (f32::from(viewport.width) - self.sidebar_width()).max(0.0),
            (f32::from(viewport.height) - tab_bar_height).max(0.0),
        )
    }

    fn pane_bounds_in_panel(&self, pane_id: usize, window: &Window) -> Option<FloatingPane> {
        if self.fullscreen == Some(pane_id) {
            return Some(workspace_maximized_pane(
                pane_id,
                self.panel_size(window),
                self.pane_gap,
            ));
        }
        if let Some(pane) = self.floating.iter().find(|pane| pane.id == pane_id) {
            return Some(pane.clone());
        }
        let rect = self
            .layout
            .as_ref()?
            .rects()
            .into_iter()
            .find_map(|(id, rect)| (id == pane_id).then_some(rect))?;
        let (panel_width, panel_height) = self.panel_size(window);
        let (inner_width, inner_height) =
            inset_panel_size((panel_width, panel_height), self.pane_gap);
        Some(FloatingPane {
            id: pane_id,
            x: self.pane_gap + rect.x * inner_width,
            y: self.pane_gap + rect.y * inner_height,
            width: (rect.width * inner_width - self.pane_gap).max(1.0),
            height: (rect.height * inner_height - self.pane_gap).max(1.0),
        })
    }

    fn pointer_in_panel(&self, event: &MouseDownEvent) -> (f32, f32) {
        self.window_position_in_panel(f32::from(event.position.x), f32::from(event.position.y))
    }

    fn window_position_in_panel(&self, x: f32, y: f32) -> (f32, f32) {
        let mut pointer = window_point_to_panel(x, y, self.sidebar_width());
        if self.has_minimized_tabs() {
            pointer.1 -= TAB_BAR_HEIGHT;
        }
        pointer
    }

    fn pointer_subject(&mut self, id: usize) -> Option<PointerSubject> {
        if let Some(index) = self.floating.iter().position(|pane| pane.id == id) {
            let pane = self.floating.remove(index);
            self.floating.push(pane.clone());
            return Some(PointerSubject::Floating(pane));
        }

        self.layout
            .as_ref()
            .filter(|layout| layout.contains(id))
            .cloned()
            .map(PointerSubject::Tiled)
    }

    fn begin_layout_animation(&mut self, from: HashMap<usize, Rect>) {
        if self.motion_speed.duration().is_none() {
            self.layout_animation = None;
            return;
        }
        self.animation_generation = self.animation_generation.wrapping_add(1);
        self.layout_animation = Some(LayoutAnimation {
            from,
            generation: self.animation_generation,
            // Match compositor behavior: the pane being manipulated stays
            // visually above its siblings for the entire transition.
            paint_last: Some(self.focused),
        });
    }

    fn normalized_panel_point(&self, pointer: (f32, f32), window: &Window) -> (f32, f32) {
        let (panel_width, panel_height) = self.panel_size(window);
        let (inner_width, inner_height) =
            inset_panel_size((panel_width, panel_height), self.pane_gap);
        (
            ((pointer.0 - self.pane_gap) / inner_width).clamp(0.0, 1.0),
            ((pointer.1 - self.pane_gap) / inner_height).clamp(0.0, 1.0),
        )
    }

    fn lift_tiled_pane(&mut self, id: usize, window: &Window) -> Option<FloatingPane> {
        let layout = self.layout.as_ref()?;
        let previous_rects = layout.rects().into_iter().collect::<HashMap<_, _>>();
        let (panel_width, panel_height) = self.panel_size(window);
        let (inner_width, inner_height) =
            inset_panel_size((panel_width, panel_height), self.pane_gap);
        let (_, rect) = layout
            .rects()
            .into_iter()
            .find(|(pane_id, _)| *pane_id == id)?;
        let pane = FloatingPane {
            id,
            x: self.pane_gap + rect.x * inner_width,
            y: self.pane_gap + rect.y * inner_height,
            width: (rect.width * inner_width - self.pane_gap)
                .max(MIN_FLOAT_WIDTH)
                .min(inner_width),
            height: (rect.height * inner_height - self.pane_gap)
                .max(MIN_FLOAT_HEIGHT)
                .min(inner_height),
        };

        self.layout = self.layout.take().and_then(|layout| layout.remove(id));
        self.floating.push(pane.clone());
        self.begin_layout_animation(previous_rects);
        Some(pane)
    }

    fn finish_pointer_drag(&mut self, pointer: (f32, f32), window: &Window) {
        let Some(drag) = self.pointer_drag.take() else {
            return;
        };
        if !matches!(drag.subject, PointerSubject::Lifted(_)) {
            return;
        }

        let dragged_pane = self
            .floating
            .iter()
            .find(|pane| pane.id == drag.pane_id)
            .cloned();
        let mut previous_rects = self
            .layout
            .as_ref()
            .map(|layout| layout.rects().into_iter().collect::<HashMap<_, _>>())
            .unwrap_or_default();
        if let Some(pane) = &dragged_pane {
            let (panel_width, panel_height) = self.panel_size(window);
            let (inner_width, inner_height) =
                inset_panel_size((panel_width, panel_height), self.pane_gap);
            previous_rects.insert(
                pane.id,
                Rect {
                    x: ((pane.x - self.pane_gap) / inner_width).clamp(0.0, 1.0),
                    y: ((pane.y - self.pane_gap) / inner_height).clamp(0.0, 1.0),
                    width: (pane.width / inner_width).clamp(0.0, 1.0),
                    height: (pane.height / inner_height).clamp(0.0, 1.0),
                },
            );
        }
        self.floating.retain(|pane| pane.id != drag.pane_id);
        let point = self.normalized_panel_point(pointer, window);
        if let Some(layout) = &mut self.layout {
            if let Some((target, axis, before)) = drop_placement(layout, point) {
                layout.split_at(target, drag.pane_id, axis, before);
            } else {
                let target = layout.pane_ids()[0];
                layout.split(target, drag.pane_id, Axis::Horizontal);
            }
        } else {
            self.layout = Some(Node::pane(drag.pane_id));
        }
        self.begin_layout_animation(previous_rects);
    }

    fn begin_pointer_interaction(
        &mut self,
        id: usize,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .workspace_transition
            .as_ref()
            .is_some_and(|transition| transition.outgoing.iter().any(|outgoing| outgoing.id == id))
        {
            return;
        }
        self.begin_pointer_interaction_focus(id, window, cx);

        if !event.modifiers.control || self.fullscreen == Some(id) {
            cx.notify();
            return;
        }

        let operation = match event.button {
            MouseButton::Left => PointerOperation::Move,
            MouseButton::Right => PointerOperation::Resize,
            _ => return,
        };
        self.start_pointer_drag(id, operation, event, cx);
    }

    fn begin_heading_drag(
        &mut self,
        id: usize,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button != MouseButton::Left {
            return;
        }
        if self
            .workspace_transition
            .as_ref()
            .is_some_and(|transition| transition.outgoing.iter().any(|outgoing| outgoing.id == id))
        {
            return;
        }
        self.begin_pointer_interaction_focus(id, window, cx);
        if self.fullscreen == Some(id) {
            cx.stop_propagation();
            return;
        }
        self.start_pointer_drag(id, PointerOperation::Move, event, cx);
    }

    fn begin_pointer_interaction_focus(
        &mut self,
        id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.capture_arrangement();
        self.layout_changed(cx);
        self.git_panel.search_focused = false;
        self.floating_animation = None;
        self.focused = id;
        self.raise_floating_pane(id);
        self.navigation_region = NavigationRegion::Terminal;
        window.focus(&self.focus_handle, cx);
        if let Some(terminal) = self
            .terminals
            .get(&id)
            .and_then(|pane| pane.session.as_ref())
        {
            terminal.focus();
        }
    }

    fn start_pointer_drag(
        &mut self,
        id: usize,
        operation: PointerOperation,
        event: &MouseDownEvent,
        cx: &mut Context<Self>,
    ) {
        if matches!(
            operation,
            PointerOperation::Resize
                | PointerOperation::ResizeEdge(_)
                | PointerOperation::ResizeCorner(_, _)
        ) {
            self.layout_animation = None;
        }
        let subject = self.pointer_subject(id);
        let Some(subject) = subject else {
            return;
        };

        self.pointer_drag = Some(PointerDrag {
            operation,
            button: event.button,
            start_pointer: self.pointer_in_panel(event),
            pane_id: id,
            subject,
            activated: false,
        });
        cx.stop_propagation();
        cx.notify();
    }

    fn focus_pane_on_hover(
        &mut self,
        id: usize,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .workspace_transition
            .as_ref()
            .is_some_and(|transition| transition.outgoing.iter().any(|outgoing| outgoing.id == id))
        {
            return;
        }
        // Keep the dragged pane focused even while it crosses other panes.
        if self.fullscreen.is_some_and(|expanded| expanded != id) {
            return;
        }
        if !self
            .restore_pointer_guard
            .allows_focus((f32::from(event.position.x), f32::from(event.position.y)))
        {
            return;
        }
        if self.pointer_drag.is_some()
            || self.terminal_scrollbar_drag.is_some()
            || self.sidebar_resizing
            || self.git_panel.resizing
        {
            return;
        }
        if self.navigation_region == NavigationRegion::Sidebar
            && self.sidebar_focus_pointer.is_some_and(|anchor| {
                !pointer_moved_from(
                    anchor,
                    (f32::from(event.position.x), f32::from(event.position.y)),
                )
            })
        {
            return;
        }
        if self.focused == id && self.navigation_region == NavigationRegion::Terminal {
            return;
        }
        self.sidebar_focus_pointer = None;
        self.focused = id;
        self.raise_floating_pane(id);
        self.navigation_region = NavigationRegion::Terminal;
        window.focus(&self.focus_handle, cx);
        if let Some(terminal) = self
            .terminals
            .get(&id)
            .and_then(|pane| pane.session.as_ref())
        {
            terminal.focus();
        }
        self.layout_changed(cx);
        cx.notify();
    }

    fn scroll_terminal(
        &mut self,
        id: usize,
        event: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.layout_mode || self.pointer_drag.is_some() {
            return;
        }

        let mouse_geometry = self.terminal_mouse_geometry(id, event, window);
        let Some(pane) = self.terminals.get_mut(&id) else {
            return;
        };
        let Some(terminal) = pane.session.as_ref() else {
            return;
        };

        let pixels = f32::from(event.delta.pixel_delta(px(TERMINAL_CELL_HEIGHT)).y);
        if pixels == 0.0 {
            return;
        }
        if pane.scroll_remainder != 0.0 && pane.scroll_remainder.signum() != pixels.signum() {
            pane.scroll_remainder = 0.0;
        }
        pane.scroll_remainder += pixels;
        let lines = (pane.scroll_remainder / TERMINAL_CELL_HEIGHT).trunc() as isize;
        if lines == 0 {
            return;
        }
        pane.scroll_remainder -= lines as f32 * TERMINAL_CELL_HEIGHT;
        if mouse_geometry.is_some_and(|(position, screen_size)| {
            terminal.report_mouse_wheel(lines, position, screen_size, event.modifiers)
        }) || terminal.scroll(-lines)
        {
            cx.stop_propagation();
        }
    }

    fn terminal_mouse_geometry(
        &self,
        id: usize,
        event: &ScrollWheelEvent,
        window: &Window,
    ) -> Option<((f32, f32), (u32, u32))> {
        let pane = self.pane_bounds_in_panel(id, window)?;
        let pointer =
            self.window_position_in_panel(f32::from(event.position.x), f32::from(event.position.y));
        let frame_inset = self.pane_gap / 2.0 + 2.0;
        let heading_height = if self.pane_headings_visible {
            38.0
        } else {
            0.0
        };
        let x = pointer.0 - pane.x - frame_inset;
        let y = pointer.1 - pane.y - frame_inset - heading_height;
        let (_, _, pixel_width, pixel_height) = self.terminal_grid_size(id, window);
        let screen_width = u32::from(pixel_width) + TERMINAL_PADDING as u32;
        let screen_height = u32::from(pixel_height) + TERMINAL_PADDING as u32;
        if x < 0.0 || y < 0.0 || x >= screen_width as f32 || y >= screen_height as f32 {
            return None;
        }
        Some(((x, y), (screen_width, screen_height)))
    }

    fn begin_terminal_scrollbar_drag(
        &mut self,
        pane_id: usize,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.terminals.get(&pane_id) else {
            return;
        };
        if pane.session.is_none() {
            return;
        }
        let Some(screen) = &pane.screen else {
            return;
        };
        let maximum_offset = screen.scroll_total.saturating_sub(screen.scroll_len) as usize;
        if maximum_offset == 0 {
            return;
        }
        let track_height = f32::from(screen.rows) * TERMINAL_CELL_HEIGHT + TERMINAL_PADDING;
        let thumb_fraction = scrollbar_thumb_fraction(screen, track_height);
        self.terminal_scrollbar_drag = Some(TerminalScrollbarPointerDrag {
            pane_id,
            start_pointer_y: f32::from(event.position.y),
            start_offset: (screen.scroll_offset as usize).min(maximum_offset),
            maximum_offset,
            travel_height: track_height * (1.0 - thumb_fraction),
        });
        self.focused = pane_id;
        self.navigation_region = NavigationRegion::Terminal;
        if let Some(pane) = self.terminals.get_mut(&pane_id)
            && !pane.scrollbar_hovered
        {
            pane.scrollbar_hovered = true;
            pane.scrollbar_fade_generation = pane.scrollbar_fade_generation.wrapping_add(1);
        }
        window.focus(&self.focus_handle, cx);
        cx.stop_propagation();
        cx.notify();
    }

    fn set_terminal_scrollbar_hover(
        &mut self,
        pane_id: usize,
        hovered: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.terminals.get_mut(&pane_id) else {
            return;
        };
        if pane.scrollbar_hovered == hovered {
            return;
        }
        pane.scrollbar_hovered = hovered;
        let dragging = self
            .terminal_scrollbar_drag
            .as_ref()
            .is_some_and(|drag| drag.pane_id == pane_id);
        if hovered || !dragging {
            pane.scrollbar_fade_generation = pane.scrollbar_fade_generation.wrapping_add(1);
        }
        cx.notify();
    }

    fn begin_terminal_selection(
        &mut self,
        pane_id: usize,
        event: &MouseDownEvent,
        cx: &mut Context<Self>,
    ) {
        if self.layout_mode || self.pointer_drag.is_some() || event.modifiers.control {
            return;
        }
        if let Some(pane) = self.terminals.get_mut(&pane_id) {
            self.terminal_selection_release = None;
            pane.selection_anchor_position = Some(event.position);
            pane.selection = None;
            cx.notify();
        }
    }

    fn drag_terminal_selection(
        &mut self,
        pane_id: usize,
        event: &DragMoveEvent<TerminalSelectionDrag>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The terminal surface also owns ordinary left-drag selection. Let a
        // compositor drag bubble to the workspace-level pointer handler instead
        // of consuming its mouse moves as selection updates.
        if self.layout_mode || self.pointer_drag.is_some() || event.event.modifiers.control {
            return;
        }
        let drag = event.drag(cx).clone();
        let Some(pane) = self.terminals.get_mut(&drag.pane_id) else {
            return;
        };
        let Some(screen) = pane.screen.as_ref() else {
            return;
        };
        let Some(selection) = drag.selection(
            pane_id,
            pane.selection_anchor_position,
            event.event.position,
            event.bounds,
            screen,
        ) else {
            return;
        };
        pane.selection = Some(selection);
        self.terminal_selection_release = Some(drag.pane_id);
        #[cfg(target_os = "linux")]
        {
            let selected = pane
                .selection
                .map(|selection| terminal_selected_text(screen, selection));
            if let Some(text) = selected.filter(|text| !text.is_empty()) {
                cx.write_to_primary(ClipboardItem::new_string(text));
            }
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn copy_terminal_text(&mut self, pane_id: usize, text: String, cx: &mut Context<Self>) {
        // Feedback belongs only to Desktop-initiated copies. Do not observe the
        // clipboard or react to harness output: those applications own their UI.
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.copied_pane = Some(pane_id);
        // Replacing the task cancels the old timeout, keeping rapid copies to a
        // single indicator and one timer, with no clipboard content retained.
        self.copied_cleanup = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1500))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.copied_pane = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn copy_selection(&mut self, _: &CopySelection, _: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = self.terminals.get(&self.focused).and_then(|pane| {
            Some(terminal_selected_text(
                pane.screen.as_ref()?,
                pane.selection?,
            ))
        }) else {
            return;
        };
        if !text.is_empty() {
            self.copy_terminal_text(self.focused, text, cx);
            cx.stop_propagation();
        }
    }

    fn paste_clipboard(&mut self, _: &PasteClipboard, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            if let Some((_, value)) = &mut self.boomux_setting_input {
                append_boomux_setting_text(value, &text);
                cx.stop_propagation();
                cx.notify();
                return;
            }
            if let Some(ResourceDialog {
                kind: ResourceDialogKind::Rename,
                value,
                busy: false,
                ..
            }) = self.resource_dialog.as_mut()
            {
                append_resource_name(value, &text);
                cx.stop_propagation();
                cx.notify();
            } else {
                self.paste_into_focused(&text, cx);
            }
        }
    }

    fn paste_primary(&mut self, _: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        #[cfg(target_os = "linux")]
        let item = cx.read_from_primary();
        #[cfg(target_os = "macos")]
        let item = cx.read_from_clipboard();
        if let Some(text) = item.and_then(|item| item.text()) {
            self.paste_into_focused(&text, cx);
        }
    }

    fn paste_into_focused(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some((_, value)) = &mut self.boomux_setting_input {
            append_boomux_setting_text(value, text);
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if self.layout_mode {
            cx.stop_propagation();
            return;
        }
        let pasted = self
            .terminals
            .get(&self.focused)
            .and_then(|pane| pane.session.as_ref())
            .is_some_and(|terminal| terminal.paste(text));
        if pasted {
            if let Some(pane) = self.terminals.get_mut(&self.focused) {
                pane.selection = None;
            }
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn on_pointer_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.sidebar_resizing {
            if event.pressed_button == Some(MouseButton::Left) {
                self.sidebar_drag_width = Some(f32::from(event.position.x).clamp(0.0, 600.0));
                if let Some(width) = sidebar_drag_target(f32::from(event.position.x)) {
                    self.sidebar_visible = true;
                    self.sidebar_preferred_width = width;
                } else {
                    self.sidebar_visible = false;
                    self.sidebar_menu = None;
                    self.sidebar_header_menu_open = false;
                    self.git_panel.search_focused = false;
                    self.nodes_open = false;
                    if self.navigation_region == NavigationRegion::Sidebar {
                        self.leave_sidebar(cx);
                    }
                }
            } else {
                self.finish_sidebar_resize();
            }
            cx.notify();
            cx.stop_propagation();
            return;
        }
        if self.git_panel.resizing {
            if event.pressed_button == Some(MouseButton::Left) {
                self.git_panel.height = Some(
                    (f32::from(window.viewport_size().height) - f32::from(event.position.y)).clamp(
                        120.0,
                        (f32::from(window.viewport_size().height) - 180.0).max(120.0),
                    ),
                );
            } else {
                self.git_panel.resizing = false;
            }
            cx.notify();
            cx.stop_propagation();
            return;
        }

        if let Some(drag) = self.terminal_scrollbar_drag.clone() {
            if event.pressed_button != Some(MouseButton::Left) {
                self.terminal_scrollbar_drag = None;
                self.terminal_selection_release = None;
                return;
            }
            let offset = scrollbar_offset_from_drag(
                drag.start_offset,
                drag.maximum_offset,
                f32::from(event.position.y) - drag.start_pointer_y,
                drag.travel_height,
            );
            if let Some(terminal) = self
                .terminals
                .get(&drag.pane_id)
                .and_then(|pane| pane.session.as_ref())
            {
                terminal.scroll_to(offset);
            }
            cx.stop_propagation();
            return;
        }
        let pointer =
            self.window_position_in_panel(f32::from(event.position.x), f32::from(event.position.y));
        let Some(mut drag) = self.pointer_drag.clone() else {
            return;
        };
        if event.pressed_button != Some(drag.button) {
            self.finish_pointer_drag(pointer, window);
            cx.notify();
            return;
        }

        let dx = pointer.0 - drag.start_pointer.0;
        let dy = pointer.1 - drag.start_pointer.1;
        if !drag.activated {
            if dx.hypot(dy) < DRAG_ACTIVATION_DISTANCE {
                return;
            }
            drag.activated = true;
            if matches!(drag.operation, PointerOperation::Move)
                && matches!(drag.subject, PointerSubject::Tiled(_))
            {
                let Some(bounds) = self.lift_tiled_pane(drag.pane_id, window) else {
                    self.pointer_drag = None;
                    return;
                };
                drag.subject = PointerSubject::Lifted(bounds);
            }
            self.pointer_drag = Some(drag.clone());
        }
        let (panel_width, panel_height) = self.panel_size(window);

        let lifted = matches!(drag.subject, PointerSubject::Lifted(_));
        match drag.subject {
            PointerSubject::Floating(start_bounds) | PointerSubject::Lifted(start_bounds) => {
                let Some(pane) = self
                    .floating
                    .iter_mut()
                    .find(|pane| pane.id == drag.pane_id)
                else {
                    self.pointer_drag = None;
                    return;
                };
                *pane = if lifted && matches!(drag.operation, PointerOperation::Move) {
                    lifted_drag_bounds(start_bounds, (dx, dy))
                } else {
                    dragged_bounds(
                        start_bounds,
                        drag.operation,
                        (dx, dy),
                        (panel_width, panel_height),
                    )
                };
            }
            PointerSubject::Tiled(start_layout) => match drag.operation {
                PointerOperation::Move => unreachable!("tiled moves are lifted before dragging"),
                PointerOperation::ResizeCorner(horizontal, vertical) => {
                    let mut layout = start_layout;
                    layout.resize_edge_from_pointer(
                        drag.pane_id,
                        horizontal,
                        dx / panel_width.max(1.0),
                    );
                    layout.resize_edge_from_pointer(
                        drag.pane_id,
                        vertical,
                        dy / panel_height.max(1.0),
                    );
                    self.layout = Some(layout);
                }
                PointerOperation::ResizeEdge(edge) => {
                    let mut layout = start_layout;
                    let delta = match edge {
                        Direction::Left | Direction::Right => dx / panel_width.max(1.0),
                        Direction::Up | Direction::Down => dy / panel_height.max(1.0),
                    };
                    layout.resize_edge_from_pointer(drag.pane_id, edge, delta);
                    self.layout = Some(layout);
                }
                PointerOperation::Resize => {
                    self.layout = Some(resized_tiled_layout(
                        start_layout,
                        drag.pane_id,
                        (dx, dy),
                        (panel_width, panel_height),
                    ));
                }
            },
        }
        cx.notify();
    }

    fn finish_sidebar_resize(&mut self) {
        let previous_width = self.sidebar_width();
        self.sidebar_resizing = false;
        self.sidebar_drag_width = None;
        if self.motion_speed.duration().is_some()
            && (previous_width - self.sidebar_width()).abs() > 1.0
        {
            self.drawer_animation_generation = self.drawer_animation_generation.wrapping_add(1);
            self.drawer_animation_from = Some(previous_width);
        }
        self.save_settings();
    }

    fn end_pointer_interaction(
        &mut self,
        event: &MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button == MouseButton::Left {
            let source_pane = self.terminal_selection_release;
            let enabled = self.copy_on_select && !self.layout_mode && self.pointer_drag.is_none();
            if let Some(text) = take_terminal_selection_copy(
                &mut self.terminal_selection_release,
                &self.terminals,
                enabled,
            ) && let Some(pane_id) = source_pane
            {
                self.copy_terminal_text(pane_id, text, cx);
            }
        }
        if self.sidebar_resizing {
            self.finish_sidebar_resize();
            cx.notify();
            cx.stop_propagation();
            return;
        }
        if self.git_panel.resizing {
            self.git_panel.resizing = false;
            cx.notify();
            cx.stop_propagation();
            return;
        }

        if let Some(drag) = self.terminal_scrollbar_drag.take() {
            if let Some(pane) = self.terminals.get_mut(&drag.pane_id)
                && !pane.scrollbar_hovered
            {
                pane.scrollbar_fade_generation = pane.scrollbar_fade_generation.wrapping_add(1);
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }
        let pointer =
            self.window_position_in_panel(f32::from(event.position.x), f32::from(event.position.y));
        self.finish_pointer_drag(pointer, window);
        self.layout_changed(cx);
        cx.notify();
    }

    fn focus_left(&mut self, _: &FocusLeft, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_direction(Direction::Left, window, cx);
    }
    fn focus_right(&mut self, _: &FocusRight, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_direction(Direction::Right, window, cx);
    }
    fn focus_up(&mut self, _: &FocusUp, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_direction(Direction::Up, window, cx);
    }
    fn focus_down(&mut self, _: &FocusDown, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_direction(Direction::Down, window, cx);
    }
    fn move_left(&mut self, _: &MoveLeft, window: &mut Window, cx: &mut Context<Self>) {
        self.move_direction(Direction::Left, window, cx);
    }
    fn move_right(&mut self, _: &MoveRight, window: &mut Window, cx: &mut Context<Self>) {
        self.move_direction(Direction::Right, window, cx);
    }
    fn move_up(&mut self, _: &MoveUp, window: &mut Window, cx: &mut Context<Self>) {
        if self.navigation_region == NavigationRegion::Sidebar {
            self.move_selected_workspace(-1, cx);
            return;
        }
        self.move_direction(Direction::Up, window, cx);
    }
    fn move_down(&mut self, _: &MoveDown, window: &mut Window, cx: &mut Context<Self>) {
        if self.navigation_region == NavigationRegion::Sidebar {
            self.move_selected_workspace(1, cx);
            return;
        }
        self.move_direction(Direction::Down, window, cx);
    }
    fn resize_left(&mut self, _: &ResizeLeft, window: &mut Window, cx: &mut Context<Self>) {
        self.resize_direction(Direction::Left, 0.04, 32.0, window, cx);
    }
    fn resize_right(&mut self, _: &ResizeRight, window: &mut Window, cx: &mut Context<Self>) {
        self.resize_direction(Direction::Right, 0.04, 32.0, window, cx);
    }
    fn resize_up(&mut self, _: &ResizeUp, window: &mut Window, cx: &mut Context<Self>) {
        self.resize_direction(Direction::Up, 0.04, 32.0, window, cx);
    }
    fn resize_down(&mut self, _: &ResizeDown, window: &mut Window, cx: &mut Context<Self>) {
        self.resize_direction(Direction::Down, 0.04, 32.0, window, cx);
    }
    fn resize_small_left(
        &mut self,
        _: &ResizeSmallLeft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.resize_direction(Direction::Left, 0.015, 12.0, window, cx);
    }
    fn resize_small_right(
        &mut self,
        _: &ResizeSmallRight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.resize_direction(Direction::Right, 0.015, 12.0, window, cx);
    }
    fn resize_small_up(&mut self, _: &ResizeSmallUp, window: &mut Window, cx: &mut Context<Self>) {
        self.resize_direction(Direction::Up, 0.015, 12.0, window, cx);
    }
    fn resize_small_down(
        &mut self,
        _: &ResizeSmallDown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.resize_direction(Direction::Down, 0.015, 12.0, window, cx);
    }
    fn resize_large_left(
        &mut self,
        _: &ResizeLargeLeft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.resize_direction(Direction::Left, 0.12, 96.0, window, cx);
    }
    fn resize_large_right(
        &mut self,
        _: &ResizeLargeRight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.resize_direction(Direction::Right, 0.12, 96.0, window, cx);
    }
    fn resize_large_up(&mut self, _: &ResizeLargeUp, window: &mut Window, cx: &mut Context<Self>) {
        self.resize_direction(Direction::Up, 0.12, 96.0, window, cx);
    }
    fn resize_large_down(
        &mut self,
        _: &ResizeLargeDown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.resize_direction(Direction::Down, 0.12, 96.0, window, cx);
    }
    fn toggle_split(&mut self, _: &ToggleSplit, _: &mut Window, cx: &mut Context<Self>) {
        self.transform_nearest_split(Node::toggle_split, cx);
    }
    fn equalize_split(&mut self, _: &EqualizeSplit, _: &mut Window, cx: &mut Context<Self>) {
        self.transform_nearest_split(Node::equalize_split, cx);
    }
    fn swap_split(&mut self, _: &SwapSplit, _: &mut Window, cx: &mut Context<Self>) {
        self.transform_nearest_split(Node::swap_split, cx);
    }
    fn align_floating_left(
        &mut self,
        _: &AlignFloatingLeft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.align_floating(FloatingAlignment::Left, window, cx);
    }
    fn align_floating_right(
        &mut self,
        _: &AlignFloatingRight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.align_floating(FloatingAlignment::Right, window, cx);
    }
    fn align_floating_up(
        &mut self,
        _: &AlignFloatingUp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.align_floating(FloatingAlignment::Up, window, cx);
    }
    fn align_floating_down(
        &mut self,
        _: &AlignFloatingDown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.align_floating(FloatingAlignment::Down, window, cx);
    }
    fn center_floating(&mut self, _: &CenterFloating, window: &mut Window, cx: &mut Context<Self>) {
        self.align_floating(FloatingAlignment::Center, window, cx);
    }
    fn cycle_pane_next(&mut self, _: &CyclePaneNext, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_pane(false, window, cx);
    }
    fn cycle_pane_previous(
        &mut self,
        _: &CyclePanePrevious,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cycle_pane(true, window, cx);
    }
    fn cycle_workspace_next(
        &mut self,
        _: &CycleWorkspaceNext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cycle_workspace(false, window, cx);
    }
    fn cycle_workspace_previous(
        &mut self,
        _: &CycleWorkspacePrevious,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cycle_workspace(true, window, cx);
    }

    fn move_sidebar_selection(
        &mut self,
        offset: isize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let visible = self.sidebar_navigation_items();
        if visible.is_empty() {
            self.sidebar_item = None;
            return;
        }
        let current = self
            .sidebar_item
            .as_ref()
            .and_then(|item| visible.iter().position(|candidate| candidate == item))
            .unwrap_or(0);
        let next = current
            .saturating_add_signed(offset)
            .min(visible.len().saturating_sub(1));
        self.sidebar_item = Some(visible[next].clone());
        self.reveal_sidebar_item(window, cx);
        cx.notify();
    }

    fn move_sidebar_to_edge(&mut self, last: bool, window: &mut Window, cx: &mut Context<Self>) {
        let visible = self.sidebar_navigation_items();
        self.sidebar_item = if last {
            visible.last().cloned()
        } else {
            visible.first().cloned()
        };
        self.reveal_sidebar_item(window, cx);
        cx.notify();
    }

    fn navigate_sidebar_tree(&mut self, expand: bool, window: &mut Window, cx: &mut Context<Self>) {
        let selected = self.sidebar_item.clone();
        match selected {
            Some(SidebarItem::Workspace(workspace_id)) => {
                if expand {
                    self.expanded_workspaces.insert(workspace_id);
                } else {
                    self.expanded_workspaces.remove(&workspace_id);
                }
            }
            Some(SidebarItem::Shell { workspace_id, .. }) if !expand => {
                self.sidebar_item = Some(SidebarItem::Workspace(workspace_id));
            }
            _ => return,
        }
        self.reconcile_sidebar_item();
        self.reveal_sidebar_item(window, cx);
        cx.notify();
    }

    fn jump_sidebar_section(
        &mut self,
        backwards: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let visible = self.sidebar_navigation_items();
        let in_agents = matches!(self.sidebar_item, Some(SidebarItem::Agent { .. }));
        let target = if in_agents {
            if backwards {
                visible
                    .iter()
                    .rev()
                    .find(|item| matches!(item, SidebarItem::Workspace(_)))
            } else {
                visible
                    .iter()
                    .find(|item| matches!(item, SidebarItem::Workspace(_)))
            }
            .or_else(|| visible.first())
        } else {
            if backwards {
                visible
                    .iter()
                    .rev()
                    .find(|item| matches!(item, SidebarItem::Agent { .. }))
            } else {
                visible
                    .iter()
                    .find(|item| matches!(item, SidebarItem::Agent { .. }))
            }
            .or_else(|| visible.first())
        };
        self.sidebar_item = target.cloned();
        self.reveal_sidebar_item(window, cx);
        cx.notify();
    }

    fn activate_sidebar_item(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.sidebar_item.clone() else {
            return;
        };
        match item {
            SidebarItem::Workspace(workspace_id) => {
                self.open_workspace(&workspace_id, None, window, cx)
            }
            SidebarItem::Shell { shell_id, .. } | SidebarItem::Agent { shell_id, .. } => {
                self.activate_sidebar_shell(&shell_id, window, cx);
            }
        }
    }

    fn sidebar_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let modifiers = event.keystroke.modifiers;
        if modifiers.control || modifiers.alt || modifiers.platform || modifiers.function {
            return;
        }
        let handled = match event.keystroke.key.as_str() {
            "up" | "k" => {
                self.move_sidebar_selection(-1, window, cx);
                true
            }
            "down" | "j" => {
                self.move_sidebar_selection(1, window, cx);
                true
            }
            "left" | "h" => {
                self.navigate_sidebar_tree(false, window, cx);
                true
            }
            "right" | "l" => {
                self.navigate_sidebar_tree(true, window, cx);
                true
            }
            "home" => {
                self.move_sidebar_to_edge(false, window, cx);
                true
            }
            "end" => {
                self.move_sidebar_to_edge(true, window, cx);
                true
            }
            "tab" => {
                self.jump_sidebar_section(modifiers.shift, window, cx);
                true
            }
            "enter" => {
                self.activate_sidebar_item(window, cx);
                true
            }
            "space" => {
                if let Some(SidebarItem::Workspace(workspace_id)) = self.sidebar_item.clone() {
                    self.toggle_workspace(&workspace_id, cx);
                } else {
                    self.activate_sidebar_item(window, cx);
                }
                true
            }
            "escape" => {
                self.leave_sidebar(cx);
                true
            }
            _ => false,
        };
        if handled {
            cx.stop_propagation();
        }
    }

    fn terminal_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.key == "space" && self.layout_leader_pressed_at.is_some() {
            self.layout_leader_release_task = None;
            cx.stop_propagation();
            return;
        }
        if self.layout_mode {
            self.layout_suppressed_keys
                .insert(event.keystroke.key.clone());
        } else if event.is_held && self.layout_suppressed_keys.contains(&event.keystroke.key) {
            cx.stop_propagation();
            return;
        } else if !event.is_held {
            self.layout_suppressed_keys.remove(&event.keystroke.key);
        }
        if self.git_panel.search_focused {
            match event.keystroke.key.as_str() {
                "escape" | "enter" => self.git_panel.search_focused = false,
                "backspace" => {
                    self.git_panel.search.pop();
                }
                _ if !event.keystroke.modifiers.control
                    && !event.keystroke.modifiers.platform
                    && !event.keystroke.modifiers.alt =>
                {
                    if let Some(text) = &event.keystroke.key_char
                        && self.git_panel.search.len() + text.len() <= 256
                    {
                        self.git_panel.search.push_str(text);
                    }
                }
                _ => (),
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }

        if self.nodes_open && self.navigation_region == NavigationRegion::Sidebar {
            let modifiers = event.keystroke.modifiers;
            if modifiers.control
                || modifiers.alt
                || modifiers.platform
                || modifiers.function
                || (event.is_held && !matches!(event.keystroke.key.as_str(), "up" | "down"))
            {
                cx.stop_propagation();
                return;
            }
            match event.keystroke.key.as_str() {
                "escape" => self.nodes_open = false,
                "up" | "down" => {
                    let nodes = self
                        .node_views
                        .iter()
                        .filter(|node| !node.local)
                        .collect::<Vec<_>>();
                    let len = nodes.len();
                    if len > 0 {
                        let current = self
                            .selected_node
                            .as_ref()
                            .and_then(|id| nodes.iter().position(|node| &node.id == id));
                        let index = match (current, event.keystroke.key.as_str()) {
                            (Some(index), "up") => (index + len - 1) % len,
                            (Some(index), _) => (index + 1) % len,
                            (None, "up") => len - 1,
                            _ => 0,
                        };
                        self.selected_node = Some(nodes[index].id.clone());
                    }
                }
                "a" => self.launch_node_action(terminal::WorkspaceLaunch::AddNode, window, cx),
                "enter" | "space" => {
                    self.expanded_node = if self.expanded_node == self.selected_node {
                        None
                    } else {
                        self.selected_node.clone()
                    };
                }
                "r" => {
                    if let Some(node) = self.node_views.iter().find(|node| Some(&node.id) == self.selected_node.as_ref()
                        && !node.local && node.health == boomux::protocol::NodeProjectionHealthCode::AuthenticationRequired) {
                        self.launch_node_action(terminal::WorkspaceLaunch::ReauthenticateNode(node.id.clone()), window, cx);
                    }
                }
                _ => {}
            }
            cx.notify();
            cx.stop_propagation();
            return;
        }
        if self.settings_restart_confirm {
            if event.keystroke.key == "escape" {
                self.settings_restart_confirm = false;
                cx.notify();
            }
            cx.stop_propagation();
            return;
        }
        if self.boomux_setting_input.is_some() {
            self.boomux_setting_key_down(event, cx);
            return;
        }
        if self.help_open {
            self.help_key_down(event, cx);
            return;
        }
        if self.resource_dialog.is_some() {
            self.resource_dialog_key_down(event, window, cx);
            return;
        }
        if self.settings_open && event.keystroke.key == "escape" {
            self.close_settings(cx);
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if self.sidebar_menu.is_some() && event.keystroke.key == "escape" {
            self.sidebar_menu = None;
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if self.project_menu_open && event.keystroke.key == "escape" {
            self.project_menu_open = false;
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if self.sidebar_header_menu_open && event.keystroke.key == "escape" {
            self.sidebar_header_menu_open = false;
            cx.stop_propagation();
            cx.notify();
            return;
        }
        let modifiers = event.keystroke.modifiers;
        if event.keystroke.key == "space"
            && modifiers.control
            && !modifiers.alt
            && !modifiers.shift
            && !modifiers.platform
            && !modifiers.function
        {
            if layout_leader_starts_press(event.is_held, self.layout_leader_pressed_at.is_some()) {
                self.toggle_layout_mode(&ToggleLayoutMode, window, cx);
            }
            cx.stop_propagation();
            return;
        }
        if self.navigation_region == NavigationRegion::Sidebar {
            self.sidebar_key_down(event, window, cx);
            return;
        }
        let Some(pane) = self.terminals.get(&self.focused) else {
            return;
        };
        let modifiers = event.keystroke.modifiers;
        let scroll_modifier = if cfg!(target_os = "macos") {
            modifiers.platform
        } else {
            modifiers.shift
        };
        if scroll_modifier && !modifiers.control && !modifiers.alt && !modifiers.function {
            let handled = match event.keystroke.key.as_str() {
                "pageup" => pane.session.as_ref().is_some_and(|terminal| {
                    terminal.scroll(-pane.screen.as_ref().map_or(1, |screen| {
                        isize::try_from(screen.scroll_len).unwrap_or(isize::MAX)
                    }))
                }),
                "pagedown" => pane.session.as_ref().is_some_and(|terminal| {
                    terminal.scroll(pane.screen.as_ref().map_or(1, |screen| {
                        isize::try_from(screen.scroll_len).unwrap_or(isize::MAX)
                    }))
                }),
                "home" => pane.session.as_ref().is_some_and(|terminal| {
                    terminal.scroll_to_top();
                    true
                }),
                "end" => pane.session.as_ref().is_some_and(|terminal| {
                    terminal.scroll_to_bottom();
                    true
                }),
                _ => false,
            };
            if handled {
                cx.stop_propagation();
                return;
            }
        }
        if desktop_keystroke(&event.keystroke, self.layout_mode) {
            return;
        }
        if self.layout_mode {
            cx.stop_propagation();
            return;
        }
        let sent = pane.session.as_ref().is_some_and(|terminal| {
            terminal.send_key(
                &event.keystroke,
                if event.is_held {
                    libghostty_vt::key::Action::Repeat
                } else {
                    libghostty_vt::key::Action::Press
                },
            )
        });
        if sent {
            if !event.is_held {
                self.terminal_pressed_keys
                    .insert(event.keystroke.key.clone(), self.focused);
            }
            if let Some(pane) = self.terminals.get_mut(&self.focused) {
                pane.selection = None;
            }
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn terminal_key_up(
        &mut self,
        event: &KeyUpEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.key == "space" && self.layout_leader_pressed_at.is_some() {
            let elapsed = self
                .layout_leader_pressed_at
                .map(|pressed| pressed.elapsed());
            // Some Wayland input paths repeat as fresh release/press pairs.
            // Keep one gesture alive until a release survives the repeat gap.
            self.layout_leader_release_task = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(LAYOUT_RELEASE_SETTLE).await;
                this.update(cx, |this, cx| {
                    this.release_layout_leader_at(elapsed, cx);
                    this.layout_leader_pressed_at = None;
                    this.layout_leader_release_task = None;
                })
                .ok();
            }));
            cx.stop_propagation();
            return;
        }
        let suppressed = self.layout_suppressed_keys.remove(&event.keystroke.key);
        let Some(pane_id) = self.terminal_pressed_keys.remove(&event.keystroke.key) else {
            if suppressed {
                cx.stop_propagation();
            }
            return;
        };
        let sent = self
            .terminals
            .get(&pane_id)
            .and_then(|pane| pane.session.as_ref())
            .is_some_and(|terminal| {
                terminal.send_key(&event.keystroke, libghostty_vt::key::Action::Release)
            });
        if sent {
            cx.stop_propagation();
        }
    }

    fn start_terminal_attachment(
        &mut self,
        pane_id: usize,
        shell: ShellChoice,
        (rows, cols, pixel_width, pixel_height): (u16, u16, u16, u16),
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.terminals.get_mut(&pane_id) else {
            return;
        };
        pane.shell = Some(shell.clone());
        pane.attaching = true;
        pane.error = None;
        self.layout_changed(cx);
        cx.notify();

        cx.spawn(async move |this, cx| {
            let attached_shell = shell.clone();
            let result = cx
                .background_spawn(async move {
                    TerminalSession::attach(shell, rows, cols, pixel_width, pixel_height)
                })
                .await;
            this.update(cx, |this, cx| {
                let Some(pane) = this.terminals.get_mut(&pane_id) else {
                    return;
                };
                pane.attaching = false;
                match result {
                    Ok(terminal) => {
                        let shell_id = terminal.shell_id.clone();
                        pane.screen = Some(terminal.screen());
                        pane.shell = Some(attached_shell);
                        pane.session = Some(terminal);
                        this.watch_terminal(pane_id, shell_id, cx);
                    }
                    Err(error) => pane.error = Some(error),
                }
                this.layout_changed(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn create_and_attach_terminal(
        &mut self,
        pane_id: usize,
        anchor: ShellChoice,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let size = self.terminal_grid_size(pane_id, window);
        if let Some(pane) = self.terminals.get_mut(&pane_id) {
            pane.attaching = true;
            pane.error = None;
        }
        self.layout_changed(cx);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let shell = terminal::create_shell(&anchor, size)?;
                    let session = match TerminalSession::attach(
                        shell.clone(),
                        size.0,
                        size.1,
                        size.2,
                        size.3,
                    ) {
                        Ok(session) => session,
                        Err(error) => {
                            let _ = terminal::close_shell(&shell.id);
                            return Err(error);
                        }
                    };
                    // The existing overview worker refreshes sidebar resources.
                    // Do not hold an attached terminal behind that extra request.
                    Ok::<_, String>((shell, session))
                })
                .await;
            this.update(cx, |this, cx| {
                let Some(pane) = this.terminals.get_mut(&pane_id) else {
                    return;
                };
                pane.attaching = false;
                match result {
                    Ok((shell, session)) => {
                        let shell_id = session.shell_id.clone();
                        pane.screen = Some(session.screen());
                        pane.shell = Some(shell);
                        pane.session = Some(session);
                        this.watch_terminal(pane_id, shell_id, cx);
                    }
                    Err(error) => pane.error = Some(error),
                }
                this.layout_changed(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn create_and_attach_workspace_terminal(
        &mut self,
        workspace_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.workspace_pane_mode == WorkspacePaneMode::Workspace {
            self.open_workspace(&workspace_id, None, window, cx);
        }
        self.sidebar_menu = None;
        self.navigation_region = NavigationRegion::Terminal;
        self.fullscreen = None;
        self.layout_animation = None;
        let mut previous_rects = self
            .layout
            .as_ref()
            .map(|layout| layout.rects().into_iter().collect::<HashMap<_, _>>())
            .unwrap_or_default();
        let pane_id = self.insert_pane();
        self.focused = pane_id;
        if self.motion_speed.duration().is_some()
            && let Some(target) = self.layout.as_ref().and_then(|layout| {
                layout
                    .rects()
                    .into_iter()
                    .find_map(|(id, rect)| (id == pane_id).then_some(rect))
            })
        {
            previous_rects.insert(
                pane_id,
                Rect {
                    x: target.x + target.width / 2.0,
                    y: 0.0,
                    width: 0.0,
                    height: 0.0,
                },
            );
            self.begin_layout_animation(previous_rects);
        }
        let size = self.terminal_grid_size(pane_id, window);
        if let Some(pane) = self.terminals.get_mut(&pane_id) {
            pane.attaching = true;
        }
        self.layout_changed(cx);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let shell = terminal::create_shell_in_workspace(&workspace_id, size)?;
                    let session = match TerminalSession::attach(
                        shell.clone(),
                        size.0,
                        size.1,
                        size.2,
                        size.3,
                    ) {
                        Ok(session) => session,
                        Err(error) => {
                            let _ = terminal::close_shell(&shell.id);
                            return Err(error);
                        }
                    };
                    // The existing overview worker refreshes sidebar resources.
                    // Do not hold an attached terminal behind that extra request.
                    Ok::<_, String>((shell, session))
                })
                .await;
            this.update(cx, |this, cx| {
                let Some(pane) = this.terminals.get_mut(&pane_id) else {
                    return;
                };
                pane.attaching = false;
                match result {
                    Ok((shell, session)) => {
                        let shell_id = session.shell_id.clone();
                        pane.screen = Some(session.screen());
                        pane.shell = Some(shell);
                        pane.session = Some(session);
                        this.watch_terminal(pane_id, shell_id, cx);
                    }
                    Err(error) => pane.error = Some(error),
                }
                this.layout_changed(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn create_and_attach_new_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.project_menu_open = false;
        self.create_workspace_terminal(terminal::WorkspaceLaunch::Shell, window, cx);
    }

    fn create_workspace_terminal(
        &mut self,
        launch: terminal::WorkspaceLaunch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sidebar_menu = None;
        self.navigation_region = NavigationRegion::Terminal;
        self.fullscreen = None;
        self.layout_animation = None;
        if self.workspace_pane_mode == WorkspacePaneMode::Workspace {
            self.detach_all_panes(window);
        }
        let pane_id = self.insert_pane();
        self.focused = pane_id;
        let size = self.terminal_grid_size(pane_id, window);
        if let Some(pane) = self.terminals.get_mut(&pane_id) {
            pane.attaching = true;
            pane.error = None;
        }
        self.layout_changed(cx);
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let (shell, setup_workspace_cleanup) =
                        terminal::create_workspace_with_shell(launch)?;
                    let mut session = match TerminalSession::attach(
                        shell.clone(),
                        size.0,
                        size.1,
                        size.2,
                        size.3,
                    ) {
                        Ok(session) => session,
                        Err(error) => {
                            // Setup ownership does not authorize deleting resources added meanwhile.
                            if !shell.desktop_setup && remote::identity(&shell.id).is_none() {
                                let _ = terminal::remove_workspace(&shell.workspace_id);
                            }
                            return Err(error);
                        }
                    };
                    session.setup_workspace_cleanup = setup_workspace_cleanup;
                    // The existing overview worker refreshes sidebar resources.
                    // Do not hold an attached terminal behind that extra request.
                    Ok::<_, String>((shell, session))
                })
                .await;
            this.update(cx, |this, cx| {
                let Some(pane) = this.terminals.get_mut(&pane_id) else {
                    return;
                };
                pane.attaching = false;
                match result {
                    Ok((shell, session)) => {
                        let shell_id = session.shell_id.clone();
                        let workspace_id = shell.workspace_id.clone();
                        pane.screen = Some(session.screen());
                        pane.shell = Some(shell);
                        pane.session = Some(session);
                        reveal_opened_workspace(
                            this.workspace_pane_mode,
                            &mut this.expanded_workspaces,
                            &workspace_id,
                        );
                        if !this.workspace_order.contains(&workspace_id) {
                            this.workspace_order.push(workspace_id.clone());
                        }
                        this.watch_terminal(pane_id, shell_id, cx);
                    }
                    Err(error) => pane.error = Some(error),
                }
                this.layout_changed(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn watch_boomux_overview(&self, window: &Window, cx: &mut Context<Self>) {
        let window_handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let result = cx
                    .background_spawn(async { terminal::refresh_overview_and_nodes() })
                    .await;
                let mut removed_setup_shells = Vec::new();
                let mut setup_workspace_cleanups = Vec::new();
                let keep_watching = this
                    .update(cx, |this, cx| {
                        let (result, nodes) = result;
                        let (node_views, nodes_error) = match nodes {
                            Ok(nodes) => (nodes, None),
                            Err(error) => {
                                let mut retained = this.node_views.clone();
                                for node in &mut retained {
                                    node.current = false;
                                }
                                (retained, Some(error))
                            }
                        };
                        if this.node_views != node_views || this.nodes_error != nodes_error {
                            let visible_change = this.nodes_open
                                || this.nodes_error != nodes_error
                                || this.node_views.len() != node_views.len()
                                || this.node_views.iter().zip(&node_views).any(
                                    |(before, after)| {
                                        before.id != after.id
                                            || before.connected() != after.connected()
                                    },
                                );
                            this.node_views = node_views;
                            this.nodes_error = nodes_error;
                            if this.selected_node.as_ref().is_some_and(|id| {
                                !this.node_views.iter().any(|node| &node.id == id)
                            }) {
                                this.selected_node = None;
                            }
                            if visible_change {
                                cx.notify();
                            }
                        }
                        if let Ok(mut overview) = result {
                            for pane in this.terminals.values_mut() {
                                if let (Some(shell), Some(session)) =
                                    (&pane.shell, &mut pane.session)
                                    && setup_pane_was_removed(
                                        shell,
                                        session.update_events().is_closed(),
                                        &overview,
                                    )
                                {
                                    removed_setup_shells.push(SidebarResource::Shell {
                                        id: shell.id.clone(),
                                        name: shell.name.clone(),
                                        workspace_id: shell.workspace_id.clone(),
                                    });
                                    if let Some(cleanup) = session.setup_workspace_cleanup.take() {
                                        setup_workspace_cleanups.push(cleanup);
                                    }
                                }
                            }
                            reconcile_workspace_order(&mut this.workspace_order, &mut overview);
                            if overview != this.boomux_overview || this.boomux_error.is_some() {
                                this.set_boomux_overview(overview);
                                this.boomux_error = None;
                                if this.navigation_region == NavigationRegion::Sidebar {
                                    this.reconcile_sidebar_item();
                                }
                                cx.notify();
                            }
                        }
                        true
                    })
                    .unwrap_or(false);
                if !keep_watching {
                    return;
                }
                let _ = window_handle.update(cx, |_, window, cx| {
                    this.update(cx, |this, cx| this.reconnect_saved_panes(window, cx))
                });
                if !removed_setup_shells.is_empty() {
                    let _ = window_handle.update(cx, |_, window, cx| {
                        this.update(cx, |this, cx| {
                            for shell in removed_setup_shells {
                                this.remove_resource_panes(&shell, window);
                            }
                            cx.notify();
                        })
                    });
                }
                if !setup_workspace_cleanups.is_empty() {
                    let errors = cx
                        .background_spawn(async move {
                            setup_workspace_cleanups
                                .into_iter()
                                .filter_map(|cleanup| {
                                    terminal::cleanup_setup_workspace(cleanup).err()
                                })
                                .collect::<Vec<_>>()
                        })
                        .await;
                    if !errors.is_empty() {
                        let _ = this.update(cx, |this, cx| {
                            this.boomux_error = Some(format!(
                                "Could not confirm setup Workspace cleanup: {}",
                                errors.join("; ")
                            ));
                            cx.notify();
                        });
                    }
                }
            }
        })
        .detach();
    }

    fn toggle_workspace(&mut self, workspace_id: &str, cx: &mut Context<Self>) {
        if !self.expanded_workspaces.remove(workspace_id) {
            self.expanded_workspaces.insert(workspace_id.to_string());
        }
        self.layout_changed(cx);
        cx.notify();
    }

    fn minimized_tab_shells(&self) -> Vec<ShellChoice> {
        if self.pane_layout_mode != PaneLayoutMode::Tabbed {
            return Vec::new();
        }
        let focused_workspace_id = self
            .terminals
            .get(&self.focused)
            .and_then(|pane| pane.shell.as_ref())
            .map(|shell| shell.workspace_id.as_str());
        let mut shells = self
            .boomux_overview
            .workspaces
            .iter()
            .filter(|workspace| {
                focused_workspace_id == Some(workspace.id.as_str())
                    || (focused_workspace_id.is_none()
                        && self.expanded_workspaces.contains(&workspace.id))
            })
            .flat_map(|workspace| workspace.shells.iter())
            .filter(|shell| self.minimized_shells.contains(&shell.id))
            .cloned()
            .collect::<Vec<_>>();
        shells.sort_by_key(|s| {
            self.layout_document
                .minimized
                .iter()
                .position(|id| id == &s.id)
                .unwrap_or(usize::MAX)
        });
        shells
    }

    fn has_minimized_tabs(&self) -> bool {
        if self.pane_layout_mode != PaneLayoutMode::Tabbed {
            return false;
        }
        let focused_workspace_id = self
            .terminals
            .get(&self.focused)
            .and_then(|pane| pane.shell.as_ref())
            .map(|shell| shell.workspace_id.as_str());
        self.boomux_overview.workspaces.iter().any(|workspace| {
            (focused_workspace_id == Some(workspace.id.as_str())
                || (focused_workspace_id.is_none()
                    && self.expanded_workspaces.contains(&workspace.id)))
                && workspace
                    .shells
                    .iter()
                    .any(|shell| self.minimized_shells.contains(&shell.id))
        })
    }

    fn detach_all_panes(&mut self, window: &mut Window) {
        self.capture_arrangement();
        for (_, pane) in self.terminals.drain() {
            for image in pane.render_images.into_values() {
                let _ = window.drop_image(image);
            }
        }
        self.layout = None;
        self.floating.clear();
        self.pointer_drag = None;
        self.terminal_scrollbar_drag = None;
        self.terminal_selection_release = None;
        self.layout_animation = None;
        self.floating_animation = None;
        self.minimizing_panes.clear();
        self.workspace_transition = None;
        self.fullscreen = None;
    }

    fn detach_terminal_ids(&mut self, pane_ids: &[usize], window: &mut Window) {
        for pane_id in pane_ids {
            if let Some(pane) = self.terminals.remove(pane_id) {
                for image in pane.render_images.into_values() {
                    let _ = window.drop_image(image);
                }
            }
        }
    }

    fn finish_current_workspace_transition(&mut self, window: &mut Window) {
        let Some(transition) = self.workspace_transition.take() else {
            return;
        };
        let pane_ids = transition
            .outgoing
            .iter()
            .map(|pane| pane.id)
            .collect::<Vec<_>>();
        self.detach_terminal_ids(&pane_ids, window);
    }

    fn finish_workspace_transition(
        &mut self,
        generation: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .workspace_transition
            .as_ref()
            .is_none_or(|transition| transition.generation != generation)
        {
            return;
        }
        self.finish_current_workspace_transition(window);
        cx.notify();
    }

    // Retain outgoing terminals and their paint state until the slide completes.
    // Both newly opened and persisted arrangements use this lifecycle.
    fn begin_workspace_transition(
        &mut self,
        slide_direction: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.finish_current_workspace_transition(window);
        let mut outgoing = self
            .layout
            .as_ref()
            .map(Node::pane_ids)
            .unwrap_or_default()
            .into_iter()
            .chain(self.floating.iter().map(|pane| pane.id))
            .filter_map(|pane_id| self.pane_bounds_in_panel(pane_id, window))
            .collect::<Vec<_>>();
        for animation in &self.minimizing_panes {
            if !outgoing.iter().any(|pane| pane.id == animation.pane_id) {
                outgoing.push(animation.from.clone());
            }
        }
        let outgoing_ids = outgoing.iter().map(|pane| pane.id).collect::<Vec<_>>();
        let should_animate = !outgoing.is_empty();

        self.layout = None;
        self.floating.clear();
        self.pointer_drag = None;
        self.terminal_scrollbar_drag = None;
        self.terminal_selection_release = None;
        self.layout_animation = None;
        self.floating_animation = None;
        self.minimizing_panes.clear();
        self.fullscreen = None;

        if let Some(duration) = self.motion_speed.duration().filter(|_| should_animate) {
            self.animation_generation = self.animation_generation.wrapping_add(1);
            let generation = self.animation_generation;
            self.workspace_transition = Some(WorkspaceTransition {
                outgoing,
                direction: slide_direction,
                generation,
                duration,
            });
            let window_handle = window.window_handle();
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(duration).await;
                let _ = window_handle.update(cx, |_, window, cx| {
                    this.update(cx, |this, cx| {
                        this.finish_workspace_transition(generation, window, cx);
                    })
                });
            })
            .detach();
        } else {
            self.detach_terminal_ids(&outgoing_ids, window);
        }
    }

    fn animate_workspace_arrival(&mut self) {
        let Some(transition) = &self.workspace_transition else {
            return;
        };
        let direction = transition.direction;
        let incoming = self
            .layout
            .as_ref()
            .map(|layout| {
                workspace_layout_rects(layout, self.fullscreen)
                    .into_iter()
                    .map(|(id, rect)| (id, shifted_workspace_rect(rect, direction)))
                    .collect()
            })
            .unwrap_or_default();
        self.begin_layout_animation(incoming);
    }

    fn open_workspace(
        &mut self,
        workspace_id: &str,
        preferred_shell_id: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.restore_workspace_layout(workspace_id, preferred_shell_id, window, cx) {
            return;
        }
        self.project_menu_open = false;
        let Some(shells) = self
            .boomux_overview
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .map(|workspace| {
                workspace
                    .shells
                    .iter()
                    .filter(|shell| !shell_is_minimized(&self.minimized_shells, &shell.id))
                    .cloned()
                    .collect::<Vec<_>>()
            })
        else {
            self.boomux_error = Some("That Boomux workspace is no longer available".into());
            cx.notify();
            return;
        };

        self.sidebar_menu = None;
        let current_workspace_id = self
            .terminals
            .get(&self.focused)
            .and_then(|pane| pane.shell.as_ref())
            .map(|shell| shell.workspace_id.clone());
        let slide_direction = workspace_slide_direction(
            &self.workspace_order,
            current_workspace_id.as_deref(),
            workspace_id,
        );
        reveal_opened_workspace(
            self.workspace_pane_mode,
            &mut self.expanded_workspaces,
            workspace_id,
        );
        if self.workspace_pane_mode == WorkspacePaneMode::Workspace {
            let desired = shells
                .iter()
                .map(|shell| shell.id.clone())
                .collect::<HashSet<_>>();
            let current = self
                .terminals
                .values()
                .filter_map(|pane| pane.shell.as_ref().map(|shell| shell.id.clone()))
                .collect::<HashSet<_>>();
            if workspace_open_replaces_panes(self.workspace_pane_mode, &current, &desired) {
                self.begin_workspace_transition(slide_direction, window, cx);
            }
        }

        let mut pane_ids = HashMap::new();
        for (pane_id, pane) in &self.terminals {
            if self
                .workspace_transition
                .as_ref()
                .is_some_and(|transition| {
                    transition
                        .outgoing
                        .iter()
                        .any(|outgoing| outgoing.id == *pane_id)
                })
            {
                continue;
            }
            if let Some(shell) = &pane.shell {
                pane_ids.insert(shell.id.clone(), *pane_id);
            }
        }

        let mut pending = Vec::new();
        for shell in shells {
            if pane_ids.contains_key(&shell.id) {
                continue;
            }
            let pane_id = self.insert_pane();
            if let Some(pane) = self.terminals.get_mut(&pane_id) {
                pane.shell = Some(shell.clone());
            }
            pane_ids.insert(shell.id.clone(), pane_id);
            pending.push((pane_id, shell));
        }

        self.animate_workspace_arrival();

        let preferred_pane = preferred_shell_id
            .and_then(|shell_id| pane_ids.get(shell_id).copied())
            .or_else(|| pane_ids.values().copied().min());
        if let Some(pane_id) = preferred_pane {
            self.focused = pane_id;
            if let Some(terminal) = self
                .terminals
                .get(&pane_id)
                .and_then(|pane| pane.session.as_ref())
            {
                terminal.focus();
            }
        }
        self.navigation_region = NavigationRegion::Terminal;
        self.sidebar_focus_pointer = None;
        window.focus(&self.focus_handle, cx);

        for (pane_id, shell) in pending {
            let size = self.terminal_grid_size(pane_id, window);
            self.start_terminal_attachment(pane_id, shell, size, cx);
        }
        self.layout_changed(cx);
        cx.notify();
    }

    fn activate_sidebar_shell(
        &mut self,
        shell_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.project_menu_open = false;
        if let Some(pane_id) = self.terminals.iter().find_map(|(pane_id, pane)| {
            if self
                .workspace_transition
                .as_ref()
                .is_some_and(|transition| {
                    transition
                        .outgoing
                        .iter()
                        .any(|outgoing| outgoing.id == *pane_id)
                })
            {
                return None;
            }
            pane.shell
                .as_ref()
                .filter(|shell| shell.id == shell_id)
                .map(|_| *pane_id)
        }) {
            let retry = self
                .terminals
                .get(&pane_id)
                .filter(|pane| pane.session.is_none() && !pane.attaching)
                .and_then(|pane| pane.shell.clone());
            if let Some(shell) = retry {
                let size = self.terminal_grid_size(pane_id, window);
                self.start_terminal_attachment(pane_id, shell, size, cx);
            }
            self.navigation_region = NavigationRegion::Terminal;
            self.focused = pane_id;
            self.raise_floating_pane(pane_id);
            if let Some(terminal) = self
                .terminals
                .get(&pane_id)
                .and_then(|pane| pane.session.as_ref())
            {
                terminal.focus();
            }
            window.focus(&self.focus_handle, cx);
            self.layout_changed(cx);
            cx.notify();
            return;
        }

        let Some(shell) = self
            .boomux_shells
            .iter()
            .find(|shell| shell.id == shell_id)
            .cloned()
        else {
            self.boomux_error = Some("That Boomux shell is no longer available".into());
            self.layout_changed(cx);
            cx.notify();
            return;
        };
        self.minimized_shells.remove(&shell.id);
        let open_workspace_ids = self
            .terminals
            .iter()
            .filter(|(id, _)| {
                self.workspace_transition.as_ref().is_none_or(|transition| {
                    !transition
                        .outgoing
                        .iter()
                        .any(|outgoing| outgoing.id == **id)
                })
            })
            .filter_map(|(_, pane)| pane.shell.as_ref().map(|shell| shell.workspace_id.clone()))
            .collect::<HashSet<_>>();
        if shell_open_replaces_panes(
            self.workspace_pane_mode,
            &open_workspace_ids,
            &shell.workspace_id,
        ) {
            if self.restore_workspace_layout(&shell.workspace_id, Some(&shell.id), window, cx) {
                return;
            }
            self.detach_all_panes(window);
        }
        self.navigation_region = NavigationRegion::Terminal;
        self.fullscreen = None;
        self.layout_animation = None;
        let mut previous_rects = self
            .layout
            .as_ref()
            .map(|layout| layout.rects().into_iter().collect::<HashMap<_, _>>())
            .unwrap_or_default();
        let pane_id = self.insert_pane();
        self.focused = pane_id;
        if self.motion_speed.duration().is_some()
            && let Some(target) = self.layout.as_ref().and_then(|layout| {
                layout
                    .rects()
                    .into_iter()
                    .find_map(|(id, rect)| (id == pane_id).then_some(rect))
            })
        {
            previous_rects.insert(
                pane_id,
                Rect {
                    x: target.x + target.width / 2.0,
                    y: 0.0,
                    width: 0.0,
                    height: 0.0,
                },
            );
            self.begin_layout_animation(previous_rects);
        }
        let size = self.terminal_grid_size(pane_id, window);
        self.start_terminal_attachment(pane_id, shell, size, cx);
    }

    fn watch_terminal(&self, pane_id: usize, shell_id: String, cx: &mut Context<Self>) {
        let Some((update_events, mut revision)) = self
            .terminals
            .get(&pane_id)
            .and_then(|pane| pane.session.as_ref())
            .filter(|terminal| terminal.shell_id == shell_id)
            .map(|terminal| (terminal.update_events(), terminal.revision()))
        else {
            return;
        };
        cx.spawn(async move |this, cx| {
            while update_events.recv().await.is_ok() {
                let keep_watching = this
                    .update(cx, |this, cx| {
                        let Some(pane) = this.terminals.get_mut(&pane_id) else {
                            return false;
                        };
                        let Some(terminal) = pane
                            .session
                            .as_ref()
                            .filter(|terminal| terminal.shell_id == shell_id)
                        else {
                            return false;
                        };
                        let next_revision = terminal.revision();
                        if next_revision != revision {
                            pane.screen = Some(terminal.screen());
                            revision = next_revision;
                            cx.notify();
                        }
                        // The worker closes the event stream after publishing its final screen.
                        // Transport closure can arrive before that last update.
                        true
                    })
                    .unwrap_or(false);
                if !keep_watching {
                    return;
                }
            }
        })
        .detach();
    }

    fn terminal_grid_size(&self, id: usize, window: &Window) -> (u16, u16, u16, u16) {
        let (width, height) = if self.fullscreen == Some(id) {
            let pane = workspace_maximized_pane(id, self.panel_size(window), self.pane_gap);
            (pane.width, pane.height)
        } else if let Some(pane) = self.floating.iter().find(|pane| pane.id == id) {
            (pane.width, pane.height)
        } else if let Some((_, rect)) = self.layout.as_ref().and_then(|layout| {
            layout
                .rects()
                .into_iter()
                .find(|(pane_id, _)| *pane_id == id)
        }) {
            let (panel_width, panel_height) = self.panel_size(window);
            let (inner_width, inner_height) =
                inset_panel_size((panel_width, panel_height), self.pane_gap);
            (
                (rect.width * inner_width - self.pane_gap).max(1.0),
                (rect.height * inner_height - self.pane_gap).max(1.0),
            )
        } else {
            (640.0, 400.0)
        };
        let content_width = (width - TERMINAL_PADDING).max(TERMINAL_CELL_WIDTH * 2.0);
        let heading_height = if self.pane_headings_visible {
            38.0
        } else {
            0.0
        };
        let content_height =
            (height - heading_height - TERMINAL_PADDING).max(TERMINAL_CELL_HEIGHT * 2.0);
        let cols = (content_width / TERMINAL_CELL_WIDTH)
            .floor()
            .clamp(2.0, f32::from(u16::MAX)) as u16;
        let rows = (content_height / TERMINAL_CELL_HEIGHT)
            .floor()
            .clamp(2.0, f32::from(u16::MAX)) as u16;
        (
            rows,
            cols,
            content_width.min(f32::from(u16::MAX)) as u16,
            content_height.min(f32::from(u16::MAX)) as u16,
        )
    }

    fn refresh_terminal_images(&mut self, window: &mut Window) {
        for pane in self.terminals.values_mut() {
            let Some(screen) = pane.screen.as_ref() else {
                continue;
            };
            if pane
                .render_image_screen
                .as_ref()
                .is_some_and(|rendered| Arc::ptr_eq(rendered, screen))
            {
                continue;
            }
            let active = screen
                .images
                .iter()
                .map(|image| image.generation)
                .collect::<HashSet<_>>();
            let stale = pane
                .render_images
                .keys()
                .filter(|generation| !active.contains(generation))
                .copied()
                .collect::<Vec<_>>();
            for generation in stale {
                if let Some(image) = pane.render_images.remove(&generation) {
                    let _ = window.drop_image(image);
                }
            }
            for terminal_image in &screen.images {
                pane.render_images
                    .entry(terminal_image.generation)
                    .or_insert_with(|| {
                        let buffer = image::RgbaImage::from_raw(
                            terminal_image.width,
                            terminal_image.height,
                            terminal_image.bgra.to_vec(),
                        )
                        .expect("validated Ghostty image dimensions");
                        Arc::new(RenderImage::new(smallvec::smallvec![image::Frame::new(
                            buffer,
                        )]))
                    });
            }
            pane.render_image_screen = Some(Arc::clone(screen));
        }
    }

    fn refresh_terminal_paint_caches(&mut self, window: &mut Window) {
        let terminal_focused = window.is_window_active()
            && self.navigation_region == NavigationRegion::Terminal
            && !self.layout_mode
            && !self.help_open
            && self.resource_dialog.is_none();
        for (id, pane) in &mut self.terminals {
            let cursor_focused = terminal_focused && *id == self.focused;
            let Some(screen) = pane.screen.as_ref() else {
                pane.paint_cache = None;
                continue;
            };
            let current = pane.paint_cache.as_ref().is_some_and(|cache| {
                Arc::ptr_eq(&cache.screen, screen)
                    && cache.selection == pane.selection
                    && cache.cursor_focused == cursor_focused
            });
            if !current {
                pane.paint_cache = Some(Arc::new(prepare_terminal_paint(
                    Arc::clone(screen),
                    pane.selection,
                    cursor_focused,
                    window,
                )));
            }
        }
    }

    fn open_nodes(&mut self, cx: &mut Context<Self>) {
        self.project_menu_open = false;
        self.git_panel.search_focused = false;
        self.sidebar_header_menu_open = false;
        self.sidebar_menu = None;
        self.nodes_open = true;
        self.git_panel.open = false;
        self.git_panel.task = None;
        self.sidebar_visible = true;
        self.navigation_region = NavigationRegion::Sidebar;
        self.selected_node = self
            .selected_node
            .take()
            .filter(|id| {
                self.node_views
                    .iter()
                    .any(|node| !node.local && node.id == *id)
            })
            .or_else(|| {
                self.node_views
                    .iter()
                    .find(|node| !node.local)
                    .map(|node| node.id.clone())
            });
        cx.notify();
    }

    fn launch_node_action(
        &mut self,
        launch: terminal::WorkspaceLaunch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.nodes_open = false;
        if self.layout_mode {
            self.leave_layout_mode(cx);
        }
        self.create_workspace_terminal(launch, window, cx);
    }

    fn forget_remote_connection(&mut self, id: String, cx: &mut Context<Self>) {
        if self.node_forget_busy || self.node_forget_confirm.as_ref() != Some(&id) {
            return;
        }
        self.node_forget_busy = true;
        self.node_forget_error = None;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    boomux::client::connect()
                        .and_then(|client| client.forget_node_registration(id))
                        .map_err(|error| error.to_string())
                })
                .await;
            this.update(cx, |this, cx| {
                this.node_forget_busy = false;
                match result {
                    Ok(_) => {
                        this.node_forget_confirm = None;
                    }
                    Err(error) => {
                        this.node_forget_error =
                            Some(format!("Could not forget connection: {error}"));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn nodes_panel(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if !self.nodes_open {
            return None;
        }
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis() as u64);
        let rows = self
            .node_views
            .iter()
            .filter(|node| !node.local)
            .map(|node| {
                let id = node.id.clone();
                div()
                    .id(SharedString::from(format!("node-row-{}", node.id)))
                    .px_2()
                    .py_2()
                    .rounded_md()
                    .cursor_pointer()
                    .bg(rgb(if self.selected_node.as_ref() == Some(&node.id) {
                        0x313244
                    } else {
                        0x1e1e2e
                    }))
                    .hover(|row| row.bg(rgb(0x313244)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.selected_node = Some(id.clone());
                        this.expanded_node = if this.expanded_node.as_ref() == Some(&id) {
                            None
                        } else {
                            Some(id.clone())
                        };
                        cx.notify();
                    }))
                    .child(div().flex().items_center().gap_2()
                        .child(div().flex_1().min_w_0().text_sm().truncate().child(node.label.clone()))
                        .child(div().flex_none().text_xs().text_color(rgb(0x7f849c))
                            .child(if self.expanded_node.as_ref() == Some(&node.id) { "▾" } else { "▸" })))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(if node.connected() { 0xa6e3a1 } else { 0xf9e2af }))
                            .child(node.status()),
                    )
            .when(self.expanded_node.as_ref() == Some(&node.id), |panel| {
                panel.child(div().mt_2().pt_2().border_t_1().border_color(rgb(0x45475a))
                    .id("remote-machine-details")
                    .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
                    .flex().flex_col().gap_2().text_xs().text_color(rgb(0xa6adc8))
                    .when_some(node.route.clone(), |detail, route| detail.child(div().truncate().child(format!("SSH · {route}"))))
                    .child(format!("{} workspace{} · {} shell{}{}", node.workspace_count,
                        if node.workspace_count == 1 { "" } else { "s" }, node.shell_count,
                        if node.shell_count == 1 { "" } else { "s" },
                        if node.connected() { "" } else { " · cached" }))
                    .when_some(node.version.clone(), |detail, version| detail.child(format!("Boomux {version}")))
                    .when(!node.connected(), |detail| detail.child(node.last_seen(now_ms)).child(node.guidance()))
                    .when(!node.local && node.connected(), |detail| {
                        let node_id = node.id.clone();
                        let name = node.label.clone();
                        let upgrade_id = node.id.clone();
                        detail.child(Self::settings_option("create-remote-workspace", "New workspace", false)
                            .flex_none()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                let name = terminal::project_workspace_name(&name, this.boomux_overview.workspaces.iter()
                                    .filter(|w| remote::identity(&w.id).is_some_and(|id| id.node_id == node_id))
                                    .map(|w| w.name.as_str()));
                                this.launch_node_action(terminal::WorkspaceLaunch::RemoteWorkspace { node_id: node_id.clone(), name }, window, cx);
                            })))
                            .child(Self::settings_option("update-remote", "Update Boomux", false)
                            .flex_none()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.launch_node_action(terminal::WorkspaceLaunch::UpgradeNode(upgrade_id.clone()), window, cx);
                            })))
                    })
                    .when(!node.local && node.health == boomux::protocol::NodeProjectionHealthCode::AuthenticationRequired, |detail| {
                        let id = node.id.clone();
                        detail.child(Self::settings_option("reauthenticate-node", "Sign in…", false)
                            .flex_none()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.launch_node_action(terminal::WorkspaceLaunch::ReauthenticateNode(id.clone()), window, cx);
                            })))
                    })
                    .child(div().mt_2().pt_2().border_t_1().border_color(rgb(0x45475a))
                        .flex().flex_col().gap_2()
                        .child(Self::settings_option("remove-remote-machine", "Remove machine & uninstall Boomux…", false)
                            .flex_none()
                            .text_color(rgb(0xf38ba8))
                            .on_click(cx.listener({
                                let id = node.id.clone();
                                move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.launch_node_action(terminal::WorkspaceLaunch::UninstallNode(id.clone()), window, cx);
                                }
                            })))
                        .child(format!("Stops all managed shells on {}. Opens a terminal for confirmation.", node.label))
                        .child(Self::settings_control("forget-remote-machine", "Forget connection only…", false, !self.node_forget_busy)
                            .flex_none()
                            .on_click(cx.listener({
                                let id = node.id.clone();
                                move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    if !this.node_forget_busy { this.node_forget_confirm = Some(id.clone()); }
                                    cx.notify();
                                }
                            })))
                        .when(self.node_forget_confirm.as_ref() == Some(&node.id), |section| {
                            section.child(div().flex().flex_col().gap_2()
                                .when_some(self.node_forget_error.clone(), |confirmation, error| confirmation.child(div().text_color(rgb(0xf38ba8)).child(error)))
                                .child("Remove this connection and its cached workspaces from this computer? This does not contact the machine, uninstall Boomux, or stop remote work.")
                                .child(Self::settings_control("confirm-forget-remote", if self.node_forget_busy { "Forgetting…" } else { "Forget connection" }, false, !self.node_forget_busy)
                                    .flex_none().text_color(rgb(0xf38ba8))
                                    .on_click(cx.listener({
                                        let id = node.id.clone();
                                        move |this, _, _, cx| {
                                            cx.stop_propagation();
                                            this.forget_remote_connection(id.clone(), cx);
                                        }
                                    })))
                                .child(Self::settings_option("cancel-forget-remote", "Cancel", false).flex_none()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        cx.stop_propagation();
                                        if !this.node_forget_busy { this.node_forget_confirm = None; }
                                        cx.notify();
                                    }))))
                        })))
            })
            })
            .collect::<Vec<_>>();
        Some(
            div()
                .id("nodes-panel")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .px_3()
                .pb_3()
                .flex()
                .flex_col()
                .gap_2()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        this.navigation_region = NavigationRegion::Sidebar;
                        window.focus(&this.focus_handle, cx);
                        cx.stop_propagation();
                        cx.notify();
                    }),
                )
                .child(
                    Self::settings_option("add-remote-node", "Connect another machine…", false)
                        .flex_none()
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.launch_node_action(terminal::WorkspaceLaunch::AddNode, window, cx);
                        })),
                )
                .when_some(self.nodes_error.clone(), |panel, error| {
                    panel.child(div().text_xs().text_color(rgb(0xf9e2af)).child(error))
                })
                .when(!self.node_views.iter().any(|node| !node.local), |panel| {
                    panel.child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x9399b2))
                            .child("Connect a machine to create your first remote workspace."),
                    )
                })
                .child(
                    div()
                        .mt_1()
                        .text_xs()
                        .text_color(rgb(0x9399b2))
                        .child("REMOTE MACHINES"),
                )
                .children(rows)
                .into_any_element(),
        )
    }

    fn toggle_project_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.project_menu_open = !self.project_menu_open;
        self.sidebar_header_menu_open = false;
        self.sidebar_menu = None;
        window.focus(&self.focus_handle, cx);
        if self.project_menu_open && !self.projects_loading {
            self.projects_loading = true;
            self.projects_result = None;
            cx.spawn(async move |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async { boomux_settings::discover_projects() })
                    .await;
                this.update(cx, |this, cx| {
                    this.projects_loading = false;
                    this.projects_result = Some(result);
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
        cx.notify();
    }

    fn open_project_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.project_menu_open = false;
        self.settings_open = true;
        self.help_open = false;
        self.nodes_open = false;
        self.boomux_settings_message = None;
        self.browse_project_folders(cx);
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn browse_project_folders(&mut self, cx: &mut Context<Self>) {
        if self.project_folder_picker_open {
            return;
        }
        if self.boomux_settings_snapshot.is_none() {
            self.project_folder_picker_pending = true;
            self.load_boomux_settings(cx);
            return;
        }
        if self.boomux_settings_busy || self.boomux_setting_input.is_some() {
            self.boomux_settings_message =
                Some("Finish saving or editing settings before choosing a folder.".into());
            cx.notify();
            return;
        }
        self.project_folder_picker_open = true;
        self.boomux_settings_message = None;
        let selection = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: true,
            prompt: Some("Add project folders".into()),
        });
        cx.spawn(async move |this, cx| {
            let result = selection.await.map_err(|error| error.to_string())
                .and_then(|result| result.map_err(|error| error.to_string()));
            this.update(cx, |this, cx| {
                this.project_folder_picker_open = false;
                match result {
                    Ok(Some(paths)) => {
                        if let Some(snapshot) = &mut this.boomux_settings_snapshot {
                            match snapshot.add_project_folders(&paths) {
                                Ok(true) => this.save_boomux_settings(cx),
                                Ok(false) => {},
                                Err(error) => this.boomux_settings_message = Some(error),
                            }
                        }
                    }
                    Ok(None) => {},
                    Err(error) => this.boomux_settings_message = Some(format!("Could not open folder picker: {error}. You can still use Edit to enter folders manually.")),
                }
                cx.notify();
            }).ok();
        }).detach();
        cx.notify();
    }

    fn project_menu(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if !self.project_menu_open {
            return None;
        }
        let mut entries = div()
            .id("project-menu-list")
            .max_h(px(320.0))
            .overflow_y_scroll();
        let mut configured = false;
        if self.projects_loading {
            entries = entries.child(
                div()
                    .p_3()
                    .text_sm()
                    .text_color(rgb(0x9399b2))
                    .child("Loading projects…"),
            );
        } else if let Some(result) = &self.projects_result {
            match result {
                Ok(discovery) => {
                    configured = discovery.roots_configured;
                    if discovery.projects.is_empty() {
                        entries =
                            entries.child(div().p_3().text_sm().text_color(rgb(0x9399b2)).child(
                                if configured {
                                    "No projects found in your folders."
                                } else {
                                    "Add a project folder to find projects here."
                                },
                            ));
                    }
                    for (index, project) in discovery.projects.iter().enumerate() {
                        let project = project.clone();
                        entries = entries.child(
                            sidebar_menu_row(("project-choice", index))
                                .h_auto()
                                .py_2()
                                .flex_col()
                                .items_start()
                                .gap_1()
                                .child(
                                    div()
                                        .w_full()
                                        .truncate()
                                        .text_sm()
                                        .child(project.name.clone()),
                                )
                                .child(
                                    div()
                                        .w_full()
                                        .truncate()
                                        .text_xs()
                                        .text_color(rgb(0x9399b2))
                                        .child(project.path.display().to_string()),
                                )
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.project_menu_open = false;
                                    if this.settings_open {
                                        this.close_settings(cx);
                                    }
                                    this.create_workspace_terminal(
                                        terminal::WorkspaceLaunch::Project {
                                            name: project.name.clone(),
                                            path: project.path.clone(),
                                        },
                                        window,
                                        cx,
                                    );
                                })),
                        );
                    }
                    for warning in &discovery.warnings {
                        entries = entries.child(
                            div()
                                .p_2()
                                .text_xs()
                                .text_color(rgb(0xf9e2af))
                                .child(warning.clone()),
                        );
                    }
                }
                Err(error) => {
                    entries = entries.child(
                        div()
                            .p_3()
                            .text_sm()
                            .text_color(rgb(0xf38ba8))
                            .child(format!("Could not load projects: {error}")),
                    )
                }
            }
        }
        Some(
            div()
                .id("project-menu")
                .role(gpui::Role::Menu)
                .aria_label("Create Workspace")
                .absolute()
                .occlude()
                .top(px(54.0))
                .left(px(10.0))
                .right(px(10.0))
                .p_1()
                .rounded_lg()
                .border_1()
                .border_color(rgb(0x45475a))
                .bg(rgb(0x1e1e2e))
                .shadow_lg()
                .child(
                    sidebar_menu_row("project-new-workspace")
                        .child("New Workspace")
                        .on_click(cx.listener(|this, _, window, cx| {
                            cx.stop_propagation();
                            if this.settings_open {
                                this.close_settings(cx);
                            }
                            this.create_and_attach_new_workspace(window, cx);
                        })),
                )
                .child(
                    sidebar_menu_row("project-new-remote-workspace")
                        .child("New remote workspace…")
                        .on_click(cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            if this.settings_open {
                                this.close_settings(cx);
                            }
                            this.open_nodes(cx);
                        })),
                )
                .child(div().mx_2().my_1().h(px(1.0)).bg(rgb(0x313244)))
                .child(
                    div()
                        .px_3()
                        .py_1()
                        .text_xs()
                        .text_color(rgb(0x9399b2))
                        .child("LOCAL PROJECTS"),
                )
                .child(entries)
                .child(div().mx_2().my_1().h(px(1.0)).bg(rgb(0x313244)))
                .child(
                    sidebar_menu_row("project-manage-folders")
                        .child(if configured {
                            "Manage project folders…"
                        } else {
                            "Add project folder…"
                        })
                        .on_click(cx.listener(|this, _, window, cx| {
                            cx.stop_propagation();
                            this.open_project_settings(window, cx);
                        })),
                )
                .into_any_element(),
        )
    }

    fn sidebar_header_menu(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        self.sidebar_header_menu_open.then(|| {
            div()
                .id("sidebar-header-menu")
                .role(gpui::Role::Menu)
                .aria_label("More actions")
                .absolute()
                .occlude()
                .top(px(54.0))
                .right(px(10.0))
                .w(px(228.0))
                .p_1()
                .rounded_lg()
                .border_1()
                .border_color(rgb(0x45475a))
                .bg(rgb(0x1e1e2e))
                .shadow_lg()
                .child(
                    sidebar_menu_row("header-menu-updates")
                        .child(if self.updates_checking {
                            "Checking for updates…"
                        } else {
                            "Check for updates"
                        })
                        .on_click(cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            this.sidebar_header_menu_open = false;
                            this.updates_status = Some("Checking for updates…".into());
                            this.dismissed_desktop_update.clear();
                            this.dismissed_boomux_update.clear();
                            this.save_settings();
                            this.check_updates(cx);
                        })),
                )
                .child(
                    sidebar_menu_row("header-menu-help")
                        .justify_between()
                        .gap_3()
                        .on_click(cx.listener(|this, _, window, cx| {
                            cx.stop_propagation();
                            this.sidebar_header_menu_open = false;
                            this.toggle_help(&ToggleHelp, window, cx);
                        }))
                        .child(div().min_w_0().flex_1().child("Keyboard shortcuts"))
                        .child(
                            div()
                                .flex_none()
                                .text_xs()
                                .text_color(rgb(0x7f849c))
                                .child("F1"),
                        ),
                )
                .child(div().mx_2().my_1().h(px(1.0)).bg(rgb(0x313244)))
                .child(
                    sidebar_menu_row("header-menu-hide-sidebar")
                        .justify_between()
                        .gap_3()
                        .on_click(cx.listener(|this, _, window, cx| {
                            cx.stop_propagation();
                            this.sidebar_header_menu_open = false;
                            this.toggle_sidebar_drawer(&ToggleSidebarDrawer, window, cx);
                        }))
                        .child(div().min_w_0().flex_1().child("Hide sidebar"))
                        .child(
                            div()
                                .flex_none()
                                .text_xs()
                                .text_color(rgb(0x7f849c))
                                .child("Layout B"),
                        ),
                )
                .into_any_element()
        })
    }

    fn sidebar(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let project_menu = self.project_menu(cx);
        let header_menu = self.sidebar_header_menu(cx);
        let settings_panel = self.settings_overlay(cx);
        let (workspaces, agents) =
            sidebar_render_sources(&self.boomux_overview, self.settings_open);
        let focused_shell_id = self
            .terminals
            .get(&self.focused)
            .and_then(|pane| pane.shell.as_ref())
            .map(|shell| shell.id.as_str());
        let open_shell_ids = self
            .terminals
            .values()
            .filter_map(|pane| pane.shell.as_ref().map(|shell| shell.id.as_str()))
            .collect::<HashSet<_>>();
        let focused_workspace_id = self
            .terminals
            .get(&self.focused)
            .and_then(|pane| pane.shell.as_ref())
            .map(|shell| shell.workspace_id.as_str());
        let workspace_offsets = if self.settings_open {
            HashMap::new()
        } else {
            sidebar_workspace_offsets(
                &self.boomux_overview,
                &self.expanded_workspaces,
                self.pane_layout_mode,
            )
        };
        let workspace_order_animation = self.workspace_order_animation.clone();
        let workspace_order_animation_duration = self.motion_speed.duration();
        let workspace_rows = workspaces
            .iter()
            .map(|workspace| {
                let workspace_id = workspace.id.clone();
                let workspace_name = workspace.name.clone();
                let workspace_item = SidebarItem::Workspace(workspace.id.clone());
                let workspace_keyboard_selected = self.navigation_region
                    == NavigationRegion::Sidebar
                    && self.sidebar_item.as_ref() == Some(&workspace_item);
                let expanded = self.expanded_workspaces.contains(&workspace.id);
                let active = focused_workspace_id == Some(workspace.id.as_str());
                let remote_identity = remote::identity(&workspace.id);
                let remote_machine = remote_identity
                    .as_ref()
                    .and_then(|id| self.node_views.iter().find(|node| node.id == id.node_id));
                let shell_count = workspace.shells.len();
                let shell_rows =
                    workspace
                        .shells
                        .iter()
                        .filter(|_| expanded && self.pane_layout_mode != PaneLayoutMode::Tabbed)
                        .cloned()
                        .map(|shell| {
                            let shell_id = shell.id.clone();
                            let shell_target = SidebarResource::Shell {
                                id: shell.id.clone(),
                                workspace_id: workspace.id.clone(),
                                name: shell.name.clone(),
                            };
                            let shell_item = SidebarItem::Shell {
                                workspace_id: workspace.id.clone(),
                                shell_id: shell.id.clone(),
                            };
                            let keyboard_selected = self.navigation_region
                                == NavigationRegion::Sidebar
                                && self.sidebar_item.as_ref() == Some(&shell_item);
                            let selected = focused_shell_id == Some(shell.id.as_str());
                            let pane_open = open_shell_ids.contains(shell.id.as_str());
                            let pane_presence = shell_pane_presence(selected, pane_open);
                            let status = shell.status_label();
                            div()
                                .id(SharedString::from(format!("sidebar-shell-{}", shell.id)))
                                .ml_8()
                                .h(px(39.0))
                                .px_2()
                                .flex()
                                .items_center()
                                .gap_2()
                                .rounded_md()
                                .anchor_scroll(
                                    keyboard_selected.then(|| self.sidebar_scroll_anchor.clone()),
                                )
                                .bg(if keyboard_selected {
                                    rgb(0x45475a)
                                } else if selected {
                                    rgb(0x252536)
                                } else {
                                    rgb(0x181825)
                                })
                                .hover(|element| element.bg(rgb(0x29293d)))
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.activate_sidebar_shell(&shell_id, window, cx);
                                }))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(
                                            if pane_presence != ShellPanePresence::Minimized {
                                                0x89b4fa
                                            } else {
                                                0x6c7086
                                            },
                                        ))
                                        .child(if self.shell_has_agent(&shell) {
                                            "✦"
                                        } else {
                                            pane_presence.glyph()
                                        }),
                                )
                                .child(
                                    div()
                                        .min_w_0()
                                        .flex_1()
                                        .flex()
                                        .flex_col()
                                        .child(
                                            div()
                                                .text_sm()
                                                .font_weight(gpui::FontWeight::NORMAL)
                                                .text_color(rgb(0xcdd6f4))
                                                .child(shell.name),
                                        )
                                        .child(div().text_xs().text_color(rgb(0x6c7086)).child(
                                            format!("{} · {status}", pane_presence.label()),
                                        )),
                                )
                                .child(
                                    div()
                                        .id(SharedString::from(format!(
                                            "sidebar-shell-menu-{}",
                                            shell.id
                                        )))
                                        .w(px(24.0))
                                        .h(px(28.0))
                                        .flex_none()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded_md()
                                        .text_color(rgb(0x7f849c))
                                        .hover(|element| {
                                            element.bg(rgb(0x45475a)).text_color(rgb(0xcdd6f4))
                                        })
                                        .on_click(cx.listener(move |this, event, window, cx| {
                                            this.open_sidebar_menu(
                                                shell_target.clone(),
                                                event,
                                                window,
                                                cx,
                                            );
                                        }))
                                        .child("⋮"),
                                )
                        })
                        .collect::<Vec<_>>();

                let workspace_target = SidebarResource::Workspace {
                    id: workspace.id.clone(),
                    name: workspace.name.clone(),
                };
                let row = div()
                    .id(SharedString::from(format!(
                        "sidebar-workspace-{}",
                        workspace.id
                    )))
                    .w_full()
                    .rounded_md()
                    .when(active, |element| {
                        element.border_l_2().border_color(rgb(0x45475a))
                    })
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "sidebar-workspace-header-{}",
                                workspace_id
                            )))
                            .anchor_scroll(
                                workspace_keyboard_selected
                                    .then(|| self.sidebar_scroll_anchor.clone()),
                            )
                            .h(px(52.0))
                            .rounded_md()
                            .bg(if active { rgb(0x313244) } else { rgb(0x252536) })
                            .px_2()
                            .flex()
                            .items_center()
                            .gap_2()
                            .when(workspace_keyboard_selected, |element| {
                                element
                                    .bg(rgb(0x45475a))
                                    .border_l_2()
                                    .border_color(rgb(0xcba6f7))
                            })
                            .hover(|element| element.bg(rgb(0x313244)))
                            .cursor_pointer()
                            .on_drag(
                                WorkspaceRowDrag {
                                    workspace_id: workspace.id.clone(),
                                    workspace_name,
                                },
                                |drag, _, _, cx| cx.new(|_| drag.clone()),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.navigation_region = NavigationRegion::Sidebar;
                                this.sidebar_item = Some(workspace_item.clone());
                                window.focus(&this.focus_handle, cx);
                                if this.workspace_pane_mode == WorkspacePaneMode::Workspace
                                    || this.pane_layout_mode == PaneLayoutMode::Tabbed
                                {
                                    this.open_workspace(&workspace_id, None, window, cx);
                                } else {
                                    this.toggle_workspace(&workspace_id, cx);
                                }
                            }))
                            .child(
                                div()
                                    .relative()
                                    .flex_none()
                                    .w(px(16.0))
                                    .h(px(14.0))
                                    .rounded(px(2.0))
                                    .border_1()
                                    .border_color(if active {
                                        rgb(0x89b4fa)
                                    } else {
                                        rgb(0x9399b2)
                                    })
                                    .when(remote_identity.is_some(), |icon| {
                                        icon.child(
                                            div()
                                                .absolute()
                                                .left(px(6.0))
                                                .top(px(13.0))
                                                .w(px(2.0))
                                                .h(px(3.0))
                                                .bg(rgb(0x9399b2)),
                                        )
                                        .child(
                                            div()
                                                .absolute()
                                                .left(px(3.0))
                                                .top(px(16.0))
                                                .w(px(8.0))
                                                .h(px(1.0))
                                                .bg(rgb(0x9399b2)),
                                        )
                                    })
                                    .when(remote_identity.is_none(), |icon| {
                                        icon.child(
                                            div()
                                                .absolute()
                                                .top_0()
                                                .bottom_0()
                                                .left(px(5.0))
                                                .w(px(1.0))
                                                .bg(if active {
                                                    rgb(0x89b4fa)
                                                } else {
                                                    rgb(0x9399b2)
                                                }),
                                        )
                                        .child(
                                            div()
                                                .absolute()
                                                .top(px(6.0))
                                                .left(px(6.0))
                                                .right_0()
                                                .h(px(1.0))
                                                .bg(if active {
                                                    rgb(0x89b4fa)
                                                } else {
                                                    rgb(0x9399b2)
                                                }),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .flex()
                                    .flex_col()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .child(workspace.name.clone()),
                                    )
                                    .child(
                                        div().truncate().text_xs().text_color(rgb(0x6c7086)).child(
                                            if let Some(node) = remote_machine {
                                                format!(
                                                    "{} · {} · {shell_count} shells",
                                                    node.label,
                                                    node.status()
                                                )
                                            } else {
                                                format!(
                                                    "{shell_count} {} · {} {}",
                                                    if shell_count == 1 {
                                                        "shell"
                                                    } else {
                                                        "shells"
                                                    },
                                                    workspace.agent_count,
                                                    if workspace.agent_count == 1 {
                                                        "agent"
                                                    } else {
                                                        "agents"
                                                    }
                                                )
                                            },
                                        ),
                                    ),
                            )
                            .child(
                                div()
                                    .id(SharedString::from(format!(
                                        "sidebar-workspace-menu-{}",
                                        workspace.id
                                    )))
                                    .w(px(26.0))
                                    .h(px(30.0))
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_md()
                                    .text_color(rgb(0x7f849c))
                                    .hover(|element| {
                                        element.bg(rgb(0x45475a)).text_color(rgb(0xcdd6f4))
                                    })
                                    .on_click(cx.listener(move |this, event, window, cx| {
                                        this.open_sidebar_menu(
                                            workspace_target.clone(),
                                            event,
                                            window,
                                            cx,
                                        );
                                    }))
                                    .child("⋮"),
                            ),
                    )
                    .when(expanded, |element| element.children(shell_rows));

                if let (Some(animation), Some(duration)) = (
                    workspace_order_animation.as_ref(),
                    workspace_order_animation_duration,
                ) {
                    let target_y = workspace_offsets
                        .get(&workspace.id)
                        .copied()
                        .unwrap_or_default();
                    let from_y = animation
                        .from
                        .get(&workspace.id)
                        .copied()
                        .unwrap_or(target_y);
                    let offset = from_y - target_y;
                    let animation_id = SharedString::from(format!(
                        "workspace-order-{}-{}",
                        animation.generation, workspace.id
                    ));
                    row.with_animation(
                        animation_id,
                        Animation::new(duration).with_easing(ease_out_quint()),
                        move |element, progress| {
                            element.relative().top(px(offset * (1.0 - progress)))
                        },
                    )
                    .into_any_element()
                } else {
                    row.into_any_element()
                }
            })
            .collect::<Vec<_>>();

        let agent_rows = agents
            .iter()
            .cloned()
            .map(|agent| self.sidebar_agent(agent, focused_shell_id, cx))
            .collect::<Vec<_>>();

        div()
            .relative()
            .w(px(self.sidebar_content_width()))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .bg(rgb(0x181825))
            .border_r_1()
            .border_color(if self.navigation_region == NavigationRegion::Sidebar {
                rgb(0xcba6f7)
            } else {
                rgb(0x313244)
            })
            .child(
                div()
                    .h(px(64.0))
                    .flex_none()
                    .px_4()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .border_b_1()
                    .border_color(rgb(0x313244))
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(sidebar_brand_mark())
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .flex()
                                    .flex_col()
                                    .child(
                                        div().font_weight(gpui::FontWeight::BOLD).child("BOOMUX"),
                                    )
                                    .child(
                                        div()
                                            .id("sidebar-node-status")
                                            .truncate()
                                            .text_xs()
                                            .text_color(rgb(0x89b4fa))
                                            .cursor_pointer()
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                cx.stop_propagation();
                                                this.open_nodes(cx);
                                            }))
                                            .child(
                                                if self.node_views.iter().any(|node| !node.local) {
                                                    let unavailable = self
                                                        .node_views
                                                        .iter()
                                                        .filter(|node| !node.connected())
                                                        .count();
                                                    if unavailable == 0 {
                                                        format!(
                                                            "{} remotes · connected",
                                                            self.node_views
                                                                .iter()
                                                                .filter(|node| !node.local)
                                                                .count()
                                                        )
                                                    } else {
                                                        format!(
                                                            "{} remotes · {} unavailable",
                                                            self.node_views
                                                                .iter()
                                                                .filter(|node| !node.local)
                                                                .count(),
                                                            unavailable
                                                        )
                                                    }
                                                } else {
                                                    format!(
                                                        "active · {} workspaces",
                                                        self.boomux_overview.workspaces.len()
                                                    )
                                                },
                                            ),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .flex_none()
                            .child(
                                sidebar_header_button(
                                    "create-workspace",
                                    "New Workspace or project",
                                    "+",
                                    self.project_menu_open,
                                )
                                .on_click(cx.listener(
                                    |this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.sidebar_header_menu_open = false;
                                        this.toggle_project_menu(window, cx);
                                    },
                                )),
                            )
                            .child(
                                sidebar_header_button(
                                    "open-settings",
                                    "Settings",
                                    "⚙",
                                    self.settings_open,
                                )
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.toggle_settings(cx);
                                    },
                                )),
                            )
                            .child(
                                sidebar_header_button(
                                    "open-sidebar-menu",
                                    "More actions",
                                    "⋯",
                                    self.sidebar_header_menu_open,
                                )
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.sidebar_header_menu_open =
                                            !this.sidebar_header_menu_open;
                                        this.project_menu_open = false;
                                        this.sidebar_menu = None;
                                        cx.notify();
                                    },
                                )),
                            ),
                    ),
            )
            .when(!self.settings_open, |sidebar| {
                sidebar.child(
                    div()
                        .id("sidebar-scroll")
                        .track_scroll(&self.sidebar_scroll_handle)
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .p_3()
                        .children(self.update_notices(cx))
                        .child(
                            div()
                                .mb_2()
                                .text_xs()
                                .font_weight(gpui::FontWeight::BOLD)
                                .text_color(rgb(0x7f849c))
                                .child("WORKSPACES"),
                        )
                        .when(workspace_rows.is_empty(), |element| {
                            element.child(
                                div()
                                    .py_4()
                                    .text_sm()
                                    .text_color(rgb(0x6c7086))
                                    .child("No Boomux workspaces"),
                            )
                        })
                        .child(
                            div()
                                .id("sidebar-workspace-list")
                                .w_full()
                                .on_drag_move(cx.listener(Self::drag_workspace))
                                .children(workspace_rows),
                        ),
                )
            })
            .when(!self.settings_open, |sidebar| {
                let available = (f32::from(window.viewport_size().height) - 180.0).max(0.0);
                let height = self
                    .git_panel
                    .height
                    .unwrap_or(f32::from(window.viewport_size().height) * 0.45)
                    .min(available);
                let attention = agents
                    .iter()
                    .filter(|agent| agent.state == AgentState::Blocked)
                    .count();
                sidebar.child(
                    div()
                        .h(px(height))
                        .min_h_0()
                        .flex_none()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .id("sidebar-activity-resize")
                                .h(px(5.0))
                                .flex_none()
                                .cursor(gpui::CursorStyle::ResizeUpDown)
                                .border_t_1()
                                .border_color(rgb(0x313244))
                                .hover(|divider| divider.bg(rgb(0x45475a)))
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _, _, cx| {
                                        this.git_panel.resizing = true;
                                        cx.stop_propagation();
                                    }),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_none()
                                .gap_3()
                                .px_3()
                                .py_1()
                                .items_center()
                                .children([0, 1, 2].map(|tab| {
                                    let selected = if self.nodes_open {
                                        tab == 2
                                    } else {
                                        tab == usize::from(self.git_panel.open)
                                    };
                                    div()
                                        .id(match tab {
                                            0 => "sidebar-agents-tab",
                                            1 => "sidebar-git-tab",
                                            _ => "sidebar-nodes-tab",
                                        })
                                        .cursor_pointer()
                                        .text_sm()
                                        .pb_1()
                                        .border_b_2()
                                        .border_color(rgb(if selected {
                                            0x89b4fa
                                        } else {
                                            0x181825
                                        }))
                                        .text_color(rgb(if selected { 0xcdd6f4 } else { 0x7f849c }))
                                        .child(if tab == 2 {
                                            "Remotes".to_owned()
                                        } else if tab == 1 {
                                            "Git".to_owned()
                                        } else if attention > 0 {
                                            format!("Agents · {attention}")
                                        } else {
                                            "Agents".to_owned()
                                        })
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            window.focus(&this.focus_handle, cx);
                                            if tab == 2 {
                                                this.open_nodes(cx);
                                            } else {
                                                this.select_git_tab(tab == 1, cx);
                                            }
                                        }))
                                }))
                                .child(div().flex_1())
                                .when(self.git_panel.open, |tabs| {
                                    tabs.child(self.render_git_controls(cx))
                                }),
                        )
                        .when_some(self.render_git_panel(cx), |section, git| section.child(git))
                        .when_some(self.nodes_panel(cx), |section, nodes| section.child(nodes))
                        .when(!self.git_panel.open && !self.nodes_open, |section| {
                            section.child(
                                div()
                                    .id("sidebar-agents-scroll")
                                    .track_scroll(&self.sidebar_agent_scroll_handle)
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .px_3()
                                    .pb_3()
                                    .when(agent_rows.is_empty(), |list| {
                                        list.child(
                                            div()
                                                .py_3()
                                                .text_sm()
                                                .text_color(rgb(0x6c7086))
                                                .child("No active Boomux agents"),
                                        )
                                    })
                                    .children(agent_rows),
                            )
                        }),
                )
            })
            .when_some(settings_panel, |element, settings| element.child(settings))
            .when_some(header_menu, |element, menu| element.child(menu))
            .when_some(project_menu, |element, menu| element.child(menu))
    }

    fn sidebar_menu_overlay(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let menu = self.sidebar_menu.as_ref()?;
        let target = menu.target.clone();
        let open_workspace_id = match &target {
            SidebarResource::Workspace { id, .. } => Some(id.clone()),
            SidebarResource::Shell { .. } => None,
        };
        let create_workspace_id = match &target {
            SidebarResource::Workspace { id, .. } => Some(id.clone()),
            SidebarResource::Shell { .. } => None,
        };
        let rename_target = target.clone();
        let remove_target = target.clone();

        Some(
            div()
                .id("sidebar-resource-menu")
                .absolute()
                .occlude()
                .left(px((self.sidebar_width() - 188.0).max(0.0)))
                .top(px(menu.top))
                .w(px(178.0))
                .p_1()
                .rounded_lg()
                .border_1()
                .border_color(rgb(0x45475a))
                .bg(rgb(0x1e1e2e))
                .shadow_lg()
                .when_some(open_workspace_id, |element, workspace_id| {
                    element.child(
                        div()
                            .id("sidebar-menu-open-workspace")
                            .h(px(34.0))
                            .px_3()
                            .flex()
                            .items_center()
                            .rounded_md()
                            .cursor_pointer()
                            .hover(|row| row.bg(rgb(0x313244)))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.open_workspace(&workspace_id, None, window, cx);
                            }))
                            .child("Open workspace"),
                    )
                })
                .when_some(create_workspace_id, |element, workspace_id| {
                    element.child(
                        div()
                            .id("sidebar-menu-create-shell")
                            .h(px(34.0))
                            .px_3()
                            .flex()
                            .items_center()
                            .rounded_md()
                            .cursor_pointer()
                            .hover(|row| row.bg(rgb(0x313244)))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.create_and_attach_workspace_terminal(
                                    workspace_id.clone(),
                                    window,
                                    cx,
                                );
                            }))
                            .child("Create shell"),
                    )
                })
                .child(
                    div()
                        .id("sidebar-menu-rename")
                        .h(px(34.0))
                        .px_3()
                        .flex()
                        .items_center()
                        .justify_between()
                        .rounded_md()
                        .cursor_pointer()
                        .hover(|row| row.bg(rgb(0x313244)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.open_resource_dialog(
                                ResourceDialogKind::Rename,
                                rename_target.clone(),
                            );
                            cx.notify();
                        }))
                        .child("Rename")
                        .child(div().text_xs().text_color(rgb(0x6c7086)).child("F2")),
                )
                .child(
                    div()
                        .id("sidebar-menu-remove")
                        .h(px(34.0))
                        .px_3()
                        .flex()
                        .items_center()
                        .rounded_md()
                        .text_color(rgb(0xf38ba8))
                        .cursor_pointer()
                        .hover(|row| row.bg(rgb(0x313244)))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.request_remove_resource(remove_target.clone(), window, cx);
                        }))
                        .child("Remove"),
                )
                .into_any_element(),
        )
    }

    fn settings_option(
        id: impl Into<gpui::ElementId>,
        label: impl Into<SharedString>,
        selected: bool,
    ) -> Stateful<Div> {
        Self::settings_control(id, label, selected, true)
    }

    fn settings_control(
        id: impl Into<gpui::ElementId>,
        label: impl Into<SharedString>,
        selected: bool,
        enabled: bool,
    ) -> Stateful<Div> {
        div()
            .id(id)
            .h(px(30.0))
            .flex_1()
            .min_w_0()
            .px_2()
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .border_1()
            .border_color(rgb(if selected { 0x89b4fa } else { 0x313244 }))
            .bg(rgb(if selected { 0x313244 } else { 0x181825 }))
            .text_xs()
            .text_color(rgb(if selected { 0xcdd6f4 } else { 0xa6adc8 }))
            .when(enabled, |button| {
                button
                    .cursor_pointer()
                    .hover(|button| button.bg(rgb(0x313244)))
            })
            .when(!enabled, |button| button.cursor_default())
            .child(label.into())
    }

    fn settings_shell(&self, id: &'static str, cx: &mut Context<Self>) -> Stateful<Div> {
        div()
            .id(id)
            .absolute()
            .occlude()
            .left_0()
            .top(px(64.0))
            .bottom_0()
            .w(px(self.sidebar_content_width()))
            .min_h_0()
            .p_4()
            .flex()
            .flex_col()
            .gap_4()
            .bg(rgb(0x181825))
            .text_sm()
            .text_color(rgb(0xcdd6f4))
            .child(
                div()
                    .h(px(44.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_base()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child("Settings"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(0x7f849c))
                                    .child("Changes save automatically"),
                            ),
                    )
                    .child(
                        div()
                            .id("close-settings-panel")
                            .size(px(28.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(0x45475a))
                            .text_color(rgb(0xa6adc8))
                            .cursor_pointer()
                            .hover(|button| button.bg(rgb(0x313244)))
                            .child("×")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.close_settings(cx);
                            })),
                    ),
            )
            .child(self.settings_status(cx))
    }

    fn settings_category(label: &'static str) -> Div {
        div()
            .mt_2()
            .px_1()
            .text_sm()
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .child(label)
    }

    fn settings_group() -> Div {
        div()
            .flex_none()
            .flex()
            .flex_col()
            .rounded_lg()
            .border_1()
            .border_color(rgb(0x313244))
            .bg(rgb(0x1e1e2e))
            .overflow_hidden()
    }

    fn settings_row() -> Div {
        div()
            .flex_none()
            .p_3()
            .flex()
            .flex_col()
            .gap_2()
            .border_b_1()
            .border_color(rgb(0x313244))
    }

    fn settings_field(label: &'static str, description: &'static str) -> Div {
        Self::settings_row()
            .child(div().text_sm().child(label))
            .when(!description.is_empty(), |row| {
                row.child(div().text_xs().text_color(rgb(0x7f849c)).child(description))
            })
    }

    fn settings_switch(
        id: impl Into<gpui::ElementId>,
        label: &'static str,
        checked: bool,
        enabled: bool,
    ) -> Stateful<Div> {
        div()
            .id(id)
            .role(gpui::Role::Switch)
            .aria_label(label)
            .aria_toggled(if checked {
                gpui::Toggled::True
            } else {
                gpui::Toggled::False
            })
            .w(px(36.0))
            .h(px(22.0))
            .flex_none()
            .p(px(3.0))
            .flex()
            .items_center()
            .rounded_full()
            .bg(rgb(if checked { 0x89b4fa } else { 0x45475a }))
            .when(checked, |track| track.justify_end())
            .when(enabled, |track| track.cursor_pointer())
            .when(!enabled, |track| track.cursor_default())
            .child(div().size(px(16.0)).rounded_full().bg(rgb(0xcdd6f4)))
    }

    fn settings_toggle_row(
        label: &'static str,
        description: &'static str,
        control: impl IntoElement,
    ) -> Div {
        Self::settings_row().child(
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(div().text_sm().child(label))
                        .when(!description.is_empty(), |text| {
                            text.child(div().text_xs().text_color(rgb(0x7f849c)).child(description))
                        }),
                )
                .child(control),
        )
    }

    fn settings_status(&self, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .flex_col()
            .gap_2()
            .when(self.boomux_settings_busy, |panel| {
                panel.child(div().text_xs().child("Saving or loading settings…"))
            })
            .when_some(self.boomux_settings_message.clone(), |panel, message| {
                panel.child(div().text_xs().text_color(rgb(0xf9e2af)).child(message))
            })
            .when(self.settings_restart_pending, |panel| {
                panel.child(
                    Self::settings_option("settings-restart", "↻ Restart to apply changes", true)
                        .on_click(cx.listener(|this, _, _, cx| {
                            if !this.boomux_settings_busy {
                                this.settings_restart_confirm = true;
                                cx.notify();
                            }
                        })),
                )
            })
            .when(self.boomux_settings_message.is_some(), |panel| {
                panel.child(
                    div()
                        .flex()
                        .gap_2()
                        .child(
                            Self::settings_option("retry-settings", "Retry save", false).on_click(
                                cx.listener(|this, _, _, cx| this.save_boomux_settings(cx)),
                            ),
                        )
                        .child(
                            Self::settings_option("reload-settings", "Reload", false).on_click(
                                cx.listener(|this, _, _, cx| this.load_boomux_settings(cx)),
                            ),
                        ),
                )
            })
    }

    fn restart_settings_service(&mut self, cx: &mut Context<Self>) {
        if self.boomux_settings_busy || !self.settings_restart_confirm {
            return;
        }
        self.settings_restart_confirm = false;
        self.boomux_settings_busy = true;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async { boomux_settings::restart() })
                .await;
            this.update(cx, |this, cx| {
                this.boomux_settings_busy = false;
                match result {
                    Ok(()) => {
                        this.settings_restart_pending = false;
                        this.boomux_settings_message = None;
                        this.save_settings();
                    }
                    Err(error) => this.boomux_settings_message = Some(error),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn settings_restart_overlay(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        self.settings_restart_confirm.then(|| {
            div().absolute().inset_0().occlude().flex().items_center().justify_center()
                .bg(rgba(0x00000099))
                .child(div().w(px(400.0)).p_4().rounded_md().border_1().border_color(rgb(0x45475a))
                    .bg(rgb(0x181825)).flex().flex_col().gap_4().text_color(rgb(0xcdd6f4))
                    .child(div().text_base().child("Restart to apply settings?"))
                    .child(div().text_sm().child("The terminal service will restart. Running shells and commands stay alive; terminal views reconnect briefly."))
                    .child(div().flex().gap_2()
                        .child(Self::settings_option("cancel-settings-restart", "Later", false)
                            .on_click(cx.listener(|this, _, _, cx| { this.settings_restart_confirm = false; cx.notify(); })))
                        .child(Self::settings_option("confirm-settings-restart", "Restart now", true)
                            .on_click(cx.listener(|this, _, _, cx| this.restart_settings_service(cx))))))
                .into_any_element()
        })
    }

    fn load_boomux_settings(&mut self, cx: &mut Context<Self>) {
        if self.boomux_settings_busy {
            return;
        }
        self.boomux_setting_input = None;
        self.boomux_settings_busy = true;
        self.boomux_settings_message = None;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async { boomux_settings::load() })
                .await;
            this.update(cx, |this, cx| {
                this.boomux_settings_busy = false;
                match result {
                    Ok(snapshot) => {
                        this.boomux_settings_snapshot = Some(snapshot);
                        this.boomux_settings_message = None;
                        if this.project_folder_picker_pending {
                            this.project_folder_picker_pending = false;
                            this.browse_project_folders(cx);
                        }
                    }
                    Err(error) => {
                        this.boomux_settings_snapshot = None;
                        this.boomux_settings_message = Some(error);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn save_boomux_settings(&mut self, cx: &mut Context<Self>) {
        if self.boomux_settings_busy || self.boomux_setting_input.is_some() {
            return;
        }
        let Some(snapshot) = self.boomux_settings_snapshot.clone() else {
            return;
        };
        if !snapshot.dirty() {
            self.boomux_settings_message = Some("No changes to save.".into());
            cx.notify();
            return;
        }
        self.boomux_settings_busy = true;
        self.boomux_settings_message = None;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let requires_restart = snapshot.restart_changed();
                    boomux_settings::save(&snapshot).map(|saved| (saved, requires_restart))
                })
                .await;
            this.update(cx, |this, cx| {
                this.boomux_settings_busy = false;
                match result {
                    Ok((snapshot, requires_restart)) => {
                        this.boomux_settings_snapshot = Some(snapshot);
                        this.boomux_settings_message = None;
                        this.settings_restart_pending |= requires_restart;
                        // Finishing a save must not interrupt someone still choosing settings.
                        this.settings_restart_confirm |= requires_restart && !this.settings_open;
                        this.save_settings();
                    }
                    Err(error) => this.boomux_settings_message = Some(error),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn accept_boomux_setting(&mut self, cx: &mut Context<Self>) {
        if let Some((index, text)) = self.boomux_setting_input.clone()
            && let Some(snapshot) = &mut self.boomux_settings_snapshot
        {
            match snapshot.set(index, Some(&text)) {
                Ok(()) => {
                    self.boomux_setting_input = None;
                    self.boomux_settings_message = None;
                    self.save_boomux_settings(cx);
                }
                Err(error) => self.boomux_settings_message = Some(error),
            }
        }
        cx.notify();
    }

    fn boomux_setting_key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "escape" => self.boomux_setting_input = None,
            "enter" if !event.keystroke.modifiers.shift => self.accept_boomux_setting(cx),
            key => {
                if let Some((index, value)) = &mut self.boomux_setting_input {
                    if key == "backspace" {
                        value.pop();
                    } else if key == "a" && event.keystroke.modifiers.control {
                        value.clear();
                    } else if key == "enter"
                        && matches!(
                            boomux_settings::FIELDS[*index].kind,
                            boomux_settings::Kind::Roots
                        )
                    {
                        append_boomux_setting_text(value, "\n");
                    } else if !event.keystroke.modifiers.control
                        && !event.keystroke.modifiers.platform
                        && !event.keystroke.modifiers.alt
                        && let Some(text) = event.keystroke.key_char.as_deref()
                    {
                        append_boomux_setting_text(value, text);
                    }
                }
            }
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn shared_settings_rows(
        &self,
        indices: &[usize],
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let mut rows = Vec::new();
        if let Some(snapshot) = &self.boomux_settings_snapshot {
            for &index in indices {
                let field = &boomux_settings::FIELDS[index];
                let enabled = snapshot.control_enabled(index);
                if matches!(field.kind, boomux_settings::Kind::Bool) {
                    let description = match field.key {
                        "notifications.enabled" => "Desktop alerts when an Agent needs attention.",
                        "notifications.blocked" => "An Agent needs your input to continue.",
                        "notifications.completed" => "An Agent reports that its work is done.",
                        "notifications.sound.enabled" => "Play a sound with Agent alerts.",
                        "recovery.resume_agents" => "Resume supported Agents after recovery.",
                        "recovery.persist_terminal_history" => {
                            "Keep terminal history across recovery."
                        }
                        _ => "",
                    };
                    let control = Self::settings_switch(
                        ("boomux-switch", index),
                        field.label,
                        snapshot.control_text(index) == "true",
                        enabled && !self.boomux_settings_busy,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.boomux_settings_busy {
                            return;
                        }
                        if let Some(snapshot) = &mut this.boomux_settings_snapshot {
                            if !snapshot.control_enabled(index) {
                                return;
                            }
                            let value = if snapshot.control_text(index) == "true" {
                                "false"
                            } else {
                                "true"
                            };
                            this.boomux_settings_message = None;
                            if let Err(error) = snapshot.set_control(index, value) {
                                this.boomux_settings_message = Some(error);
                            } else {
                                if this
                                    .boomux_setting_input
                                    .as_ref()
                                    .is_some_and(|(field, _)| !snapshot.control_enabled(*field))
                                {
                                    this.boomux_setting_input = None;
                                }
                                this.save_boomux_settings(cx);
                            }
                        }
                        cx.notify();
                    }));
                    rows.push(
                        Self::settings_toggle_row(field.label, description, control)
                            .when(!enabled, |row| row.opacity(0.4))
                            .into_any_element(),
                    );
                    continue;
                }
                let mut row =
                    Self::settings_field(field.label, "").when(!enabled, |row| row.opacity(0.4));
                if self
                    .boomux_setting_input
                    .as_ref()
                    .is_some_and(|(field, _)| *field == index)
                {
                    let text = self.boomux_setting_input.as_ref().unwrap().1.clone();
                    row = row
                        .child(
                            div()
                                .p_2()
                                .min_h(px(32.0))
                                .max_h(px(140.0))
                                .overflow_hidden()
                                .rounded_md()
                                .border_1()
                                .border_color(rgb(0x89b4fa))
                                .bg(rgb(0x181825))
                                .text_sm()
                                .child(if text.is_empty() {
                                    "Type a value…".into()
                                } else {
                                    text
                                }),
                        )
                        .child(div().text_xs().text_color(rgb(0x7f849c)).child(
                            if matches!(field.kind, boomux_settings::Kind::Roots) {
                                "Enter: accept · Esc: cancel · Ctrl+A: clear · Shift+Enter: new folder."
                            } else {
                                "Enter: accept · Esc: cancel · Ctrl+A: clear."
                            },
                        ))
                        .child(
                            Self::settings_option(("accept-boomux-field", index), "Done", true)
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.accept_boomux_setting(cx)),
                                ),
                        );
                } else {
                    let options: Vec<(&str, Option<&str>)> = match field.kind {
                        boomux_settings::Kind::Bool => {
                            vec![("On", Some("true")), ("Off", Some("false"))]
                        }
                        boomux_settings::Kind::Choice(values) => values
                            .iter()
                            .map(|value| {
                                (
                                    match *value {
                                        "disabled" => "Off",
                                        "hyprland-special" => "Special workspace",
                                        other => other,
                                    },
                                    Some(*value),
                                )
                            })
                            .collect(),
                        _ => vec![("Edit", Some("edit"))],
                    };
                    if !matches!(
                        field.kind,
                        boomux_settings::Kind::Bool | boomux_settings::Kind::Choice(_)
                    ) {
                        let preview = snapshot
                            .value(field.key)
                            .map(|value| {
                                if let Some(values) = value.as_array() {
                                    match values.len() {
                                        0 => "No folders selected".into(),
                                        1 => values
                                            .get(0)
                                            .and_then(|value| value.as_str())
                                            .unwrap_or("")
                                            .chars()
                                            .take(80)
                                            .collect(),
                                        count => format!("{count} folders"),
                                    }
                                } else if let Some(text) = value.as_str() {
                                    if text.is_empty() {
                                        "System terminal".into()
                                    } else {
                                        text.chars().take(80).collect()
                                    }
                                } else {
                                    value.to_string()
                                }
                            })
                            .unwrap_or_else(|| "Not set".into());
                        row = row.child(
                            div()
                                .p_2()
                                .rounded_md()
                                .border_1()
                                .border_color(rgb(0x313244))
                                .bg(rgb(0x181825))
                                .text_xs()
                                .text_color(rgb(0xa6adc8))
                                .child(preview),
                        );
                    }
                    let mut buttons = div().flex().gap_2();
                    for (option_index, (label, value)) in options.into_iter().enumerate() {
                        let selected = match value {
                            None => snapshot.value(field.key).is_none(),
                            Some("edit") => false,
                            Some(value) => snapshot.control_text(index) == value,
                        };
                        buttons = buttons.child(
                            Self::settings_control(
                                (SharedString::from(format!("boomux-field-{index}")), option_index),
                                label, selected, enabled,
                            )
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    if this.boomux_settings_busy || !enabled {
                                        return;
                                    }
                                    this.boomux_settings_message = None;
                                    if let Some(snapshot) = &mut this.boomux_settings_snapshot {
                                        if value == Some("edit") {
                                            let text = snapshot.text(index);
                                            if text.len() > 16384 {
                                                this.boomux_settings_message = Some("This value is too large for the Desktop editor. Use boomux config edit.".into());
                                            } else {
                                                this.boomux_setting_input = Some((index, text));
                                                window.focus(&this.focus_handle, cx);
                                            }
                                        } else if value.is_some_and(|value| snapshot.control_text(index) != value) {
                                            if let Err(error) = snapshot.set_control(index, value.unwrap()) {
                                                this.boomux_settings_message = Some(error);
                                            } else {
                                                if this.boomux_setting_input.as_ref().is_some_and(|(field, _)| !snapshot.control_enabled(*field)) {
                                                    this.boomux_setting_input = None;
                                                }
                                                this.save_boomux_settings(cx);
                                            }
                                        }
                                    }
                                    cx.notify();
                                })),
                        );
                    }
                    row = row.child(buttons);
                }
                rows.push(row.into_any_element());
            }
        }
        rows
    }

    fn settings_overlay(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if !self.settings_open {
            return None;
        }
        {
            let layout = Self::settings_group()
                .child(
                    Self::settings_field("Pane layout", "Choose how open Shells are arranged.")
                        .child(
                            div()
                                .flex()
                                .gap_1()
                                .child(
                                    Self::settings_control(
                                        "pane-layout-tiled",
                                        "Tree",
                                        self.pane_layout_mode == PaneLayoutMode::Tiled,
                                        true,
                                    )
                                    .on_click(cx.listener(
                                        |this, _, window, cx| {
                                            this.set_pane_layout_mode(
                                                PaneLayoutMode::Tiled,
                                                window,
                                                cx,
                                            );
                                        },
                                    )),
                                )
                                .child(
                                    Self::settings_control(
                                        "pane-layout-tabbed",
                                        "Tabs",
                                        self.pane_layout_mode == PaneLayoutMode::Tabbed,
                                        true,
                                    )
                                    .on_click(cx.listener(
                                        |this, _, window, cx| {
                                            this.set_pane_layout_mode(
                                                PaneLayoutMode::Tabbed,
                                                window,
                                                cx,
                                            );
                                        },
                                    )),
                                ),
                        ),
                )
                .child(
                    Self::settings_field("Pane scope", "")
                        .child(
                            div()
                                .flex()
                                .gap_1()
                                .child(
                                    Self::settings_control(
                                        "pane-scope-workspace",
                                        "Workspace",
                                        self.workspace_pane_mode == WorkspacePaneMode::Workspace,
                                        true,
                                    )
                                    .on_click(cx.listener(
                                        |this, _, window, cx| {
                                            this.capture_arrangement();
                                            this.workspace_pane_mode = WorkspacePaneMode::Workspace;
                                            let workspace_id = this
                                                .terminals
                                                .get(&this.focused)
                                                .and_then(|pane| pane.shell.as_ref())
                                                .map(|shell| shell.workspace_id.clone());
                                            if let Some(workspace_id) = workspace_id {
                                                this.open_workspace(
                                                    &workspace_id,
                                                    None,
                                                    window,
                                                    cx,
                                                );
                                            } else {
                                                cx.notify();
                                            }
                                            this.save_settings();
                                        },
                                    )),
                                )
                                .child(
                                    Self::settings_control(
                                        "pane-scope-mixed",
                                        "Mixed",
                                        self.workspace_pane_mode == WorkspacePaneMode::Mixed,
                                        pane_layout_supports_scope(
                                            self.pane_layout_mode,
                                            WorkspacePaneMode::Mixed,
                                        ),
                                    )
                                    .on_click(cx.listener(
                                        |this, _, window, cx| {
                                            if pane_layout_supports_scope(
                                                this.pane_layout_mode,
                                                WorkspacePaneMode::Mixed,
                                            ) {
                                                this.capture_arrangement();
                                                this.workspace_pane_mode = WorkspacePaneMode::Mixed;
                                                if let Some(saved) = this
                                                    .layout_document
                                                    .arrangements
                                                    .get("mixed")
                                                    .cloned()
                                                {
                                                    this.layout_document.active = "mixed".into();
                                                    this.restore_arrangement(saved, window, cx);
                                                }
                                                this.layout_changed(cx);
                                                this.save_settings();
                                                cx.notify();
                                            }
                                        },
                                    )),
                                ),
                        )
                        .child(div().text_xs().text_color(rgb(0x7f849c)).child(
                            if self.pane_layout_mode == PaneLayoutMode::Tabbed {
                                "Tabs keeps each Workspace separate; Mixed is unavailable."
                            } else if self.workspace_pane_mode == WorkspacePaneMode::Workspace {
                                "Opening a Workspace replaces the canvas with its Shells."
                            } else {
                                "Shells from different Workspaces share the canvas."
                            },
                        )),
                );
            let appearance = Self::settings_group()
                .child(Self::settings_toggle_row(
                    "Window headings",
                    "Show a heading above each pane.",
                    Self::settings_switch(
                        "pane-headings",
                        "Window headings",
                        self.pane_headings_visible,
                        true,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.pane_headings_visible = !this.pane_headings_visible;
                        this.save_settings();
                        cx.notify();
                    })),
                ))
                .child(
                    Self::settings_field("Window edges", "").child(
                        div()
                            .flex()
                            .gap_1()
                            .child(
                                Self::settings_control(
                                    "pane-edges-rounded",
                                    "Rounded",
                                    self.pane_corner_style == PaneCornerStyle::Rounded,
                                    true,
                                )
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.pane_corner_style = PaneCornerStyle::Rounded;
                                        this.save_settings();
                                        cx.notify();
                                    },
                                )),
                            )
                            .child(
                                Self::settings_control(
                                    "pane-edges-square",
                                    "Square",
                                    self.pane_corner_style == PaneCornerStyle::Square,
                                    true,
                                )
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.pane_corner_style = PaneCornerStyle::Square;
                                        this.save_settings();
                                        cx.notify();
                                    },
                                )),
                            )
                            .child(
                                Self::settings_control(
                                    "pane-edges-mixed",
                                    "Mixed",
                                    self.pane_corner_style == PaneCornerStyle::Mixed,
                                    true,
                                )
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.pane_corner_style = PaneCornerStyle::Mixed;
                                        this.save_settings();
                                        cx.notify();
                                    },
                                )),
                            ),
                    ),
                )
                .child(
                    Self::settings_field("Window spacing", "Space between panes.").child(
                        div()
                            .flex()
                            .gap_1()
                            .child(
                                Self::settings_control("decrease-pane-gap", "−", false, true)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.pane_gap = (this.pane_gap - 2.0).max(0.0);
                                        this.save_settings();
                                        cx.notify();
                                    }))
                                    .flex_none()
                                    .w(px(30.0)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .h(px(30.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .text_sm()
                                    .child(format!("{:.0} px", self.pane_gap)),
                            )
                            .child(
                                Self::settings_control("increase-pane-gap", "+", false, true)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.pane_gap = (this.pane_gap + 2.0).min(32.0);
                                        this.save_settings();
                                        cx.notify();
                                    }))
                                    .flex_none()
                                    .w(px(30.0)),
                            ),
                    ),
                )
                .child(Self::settings_toggle_row(
                    "Layout overlay",
                    "Dim terminals and show animated tiles in layout mode.",
                    Self::settings_switch(
                        "layout-overlay",
                        "Layout overlay",
                        self.layout_overlay_visible,
                        true,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.layout_overlay_visible = !this.layout_overlay_visible;
                        this.save_settings();
                        cx.notify();
                    })),
                ))
                .child(
                    Self::settings_field("Motion", "Speed of pane transitions.").child(
                        div()
                            .flex()
                            .gap_1()
                            .child(
                                Self::settings_control(
                                    "motion-instant",
                                    "Instant",
                                    self.motion_speed == MotionSpeed::Instant,
                                    true,
                                )
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.motion_speed = MotionSpeed::Instant;
                                        this.layout_animation = None;
                                        this.floating_animation = None;
                                        this.save_settings();
                                        cx.notify();
                                    },
                                )),
                            )
                            .child(
                                Self::settings_control(
                                    "motion-fast",
                                    "Fast",
                                    self.motion_speed == MotionSpeed::Fast,
                                    true,
                                )
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.motion_speed = MotionSpeed::Fast;
                                        this.save_settings();
                                        cx.notify();
                                    },
                                )),
                            )
                            .child(
                                Self::settings_control(
                                    "motion-smooth",
                                    "Smooth",
                                    self.motion_speed == MotionSpeed::Smooth,
                                    true,
                                )
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.motion_speed = MotionSpeed::Smooth;
                                        this.save_settings();
                                        cx.notify();
                                    },
                                )),
                            ),
                    ),
                )
                .child(
                    Self::settings_field(
                        "Focus highlight",
                        "Strength of the active pane highlight.",
                    )
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .child(
                                Self::settings_control(
                                    "decrease-focus-highlight",
                                    "−",
                                    false,
                                    true,
                                )
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.focus_highlight_strength =
                                        this.focus_highlight_strength.saturating_sub(10);
                                    this.save_settings();
                                    cx.notify();
                                }))
                                .flex_none()
                                .w(px(30.0)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .h(px(30.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .text_sm()
                                    .child(format!("{}%", self.focus_highlight_strength)),
                            )
                            .child(
                                Self::settings_control(
                                    "increase-focus-highlight",
                                    "+",
                                    false,
                                    true,
                                )
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.focus_highlight_strength =
                                        this.focus_highlight_strength.saturating_add(10).min(100);
                                    this.save_settings();
                                    cx.notify();
                                }))
                                .flex_none()
                                .w(px(30.0)),
                            ),
                    ),
                );
            let content = div().flex_none().flex().flex_col().gap_2()
                .when_some(self.settings_error.clone(), |panel, error| panel.child(
                    div().p_3().rounded_md().bg(rgb(0x1e1e2e)).text_xs().text_color(rgb(0xf38ba8))
                        .child(format!("Settings could not be saved or loaded: {error}. Check settings.toml and restart."))
                ))
                .child(Self::settings_category("Layout & workspaces"))
                .child(layout)
                .child(Self::settings_category("Appearance"))
                .child(appearance)
                .child(Self::settings_category("Clipboard"))
                .child(Self::settings_group().child(Self::settings_toggle_row(
                    "Copy on select",
                    "Copy selected terminal text to the clipboard when you release the mouse.",
                    Self::settings_switch("copy-on-select", "Copy on select", self.copy_on_select, true)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.copy_on_select = !this.copy_on_select;
                            this.save_settings();
                            cx.notify();
                        })),
                )))
                .child(Self::settings_category("Notifications & sounds"))
                .child(Self::settings_group().children(self.shared_settings_rows(&[0, 1, 2, 3, 4, 5], cx)))
                .child(Self::settings_category("Recovery & history"))
                .child(Self::settings_group().children(self.shared_settings_rows(&[6, 7], cx)))
                .child(Self::settings_category("Safety"))
                .child(Self::settings_group().child(Self::settings_toggle_row("Confirm removals", "Ask before permanently removing a Shell or Workspace.", Self::settings_switch("removal-confirmation", "Confirm removals", self.confirm_destructive_actions, true).on_click(cx.listener(|this, _, _, cx| { this.confirm_destructive_actions = !this.confirm_destructive_actions; this.save_settings(); cx.notify(); })))))
                .child(Self::settings_category("Projects"))
                .child(div().text_xs().text_color(rgb(0x9399b2)).child("Scan these folders for projects to open from the + menu. Local Node only; no restart needed."))
                .child(Self::settings_group()
                    .child(Self::settings_row().child(Self::settings_control("browse-project-folders", "Browse for folders…", false, !self.boomux_settings_busy && !self.project_folder_picker_open)
                        .on_click(cx.listener(|this, _, _, cx| this.browse_project_folders(cx)))))
                    .children(self.shared_settings_rows(&[10, 11], cx)))
                .child(Self::settings_category("Advanced"))
                .child(Self::settings_group().child(
                    Self::settings_field("Core configuration", "Open in your configured editor. Changes are validated before saving.")
                        .when_some(self.boomux_settings_snapshot.as_ref(), |row, snapshot| row.child(
                            div().text_xs().text_color(rgb(0x7f849c))
                                .child(snapshot.path.display().to_string())
                        ))
                        .child(Self::settings_control("open-config-file", "Open config file", false, !self.boomux_settings_busy)
                            .on_click(cx.listener(|this, _, window, cx| {
                                if this.boomux_settings_busy { return; }
                                this.close_settings(cx);
                                this.create_workspace_terminal(terminal::WorkspaceLaunch::ConfigEdit, window, cx);
                            })))
                ))
                .child(Self::settings_control("manual-setup", "Open advanced setup in terminal", false, true)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.close_settings(cx);
                        this.create_workspace_terminal(terminal::WorkspaceLaunch::Setup, window, cx);
                    })));
            let body = div()
                .id("settings-list")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(content.child(div().h(px(16.0)).flex_none()));
            Some(
                self.settings_shell("settings", cx)
                    .child(body)
                    .into_any_element(),
            )
        }
    }

    fn resource_dialog_overlay(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let dialog = self.resource_dialog.as_ref()?;
        let kind_label = dialog.target.kind_label();
        let title = match dialog.kind {
            ResourceDialogKind::Rename => format!("Rename {kind_label}"),
            ResourceDialogKind::Remove => format!("Remove {kind_label}?"),
        };
        let detail = match (&dialog.target, dialog.kind) {
            (_, ResourceDialogKind::Rename) => {
                "Type a new name, then press Enter to save it.".to_string()
            }
            (SidebarResource::Shell { name, .. }, ResourceDialogKind::Remove) => format!(
                "This permanently terminates and removes the Boomux Shell “{name}”. Ctrl+W only detaches its pane."
            ),
            (SidebarResource::Workspace { id, name }, ResourceDialogKind::Remove) => {
                let shell_count = self
                    .boomux_overview
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.id == *id)
                    .map_or(0, |workspace| workspace.shells.len());
                format!(
                    "This permanently removes “{name}” and its {shell_count} Boomux shell{}.",
                    if shell_count == 1 { "" } else { "s" }
                )
            }
        };
        let busy = dialog.busy;
        let confirm_label = match (dialog.kind, busy) {
            (ResourceDialogKind::Rename, false) => "Rename",
            (ResourceDialogKind::Rename, true) => "Renaming…",
            (ResourceDialogKind::Remove, false) => "Remove",
            (ResourceDialogKind::Remove, true) => "Removing…",
        };

        Some(
            div()
                .id("resource-dialog-backdrop")
                .absolute()
                .occlude()
                .left_0()
                .top_0()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(rgba(0x000000aa))
                .child(
                    div()
                        .id("resource-dialog")
                        .w(px(440.0))
                        .p_5()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .rounded_lg()
                        .border_1()
                        .border_color(rgb(0x45475a))
                        .bg(rgb(0x1e1e2e))
                        .shadow_lg()
                        .child(
                            div()
                                .text_lg()
                                .font_weight(gpui::FontWeight::BOLD)
                                .child(title),
                        )
                        .child(div().text_sm().text_color(rgb(0xa6adc8)).child(detail))
                        .when(dialog.kind == ResourceDialogKind::Rename, |element| {
                            element.child(
                                div()
                                    .h(px(40.0))
                                    .px_3()
                                    .flex()
                                    .items_center()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(rgb(0x89b4fa))
                                    .bg(rgb(0x11111b))
                                    .child(format!(
                                        "{}{}",
                                        dialog.value,
                                        if busy { "" } else { "▏" }
                                    )),
                            )
                        })
                        .when_some(dialog.error.clone(), |element, error| {
                            element.child(div().text_sm().text_color(rgb(0xf38ba8)).child(error))
                        })
                        .child(
                            div()
                                .mt_2()
                                .flex()
                                .justify_end()
                                .gap_2()
                                .child(
                                    div()
                                        .id("resource-dialog-cancel")
                                        .h(px(34.0))
                                        .px_4()
                                        .flex()
                                        .items_center()
                                        .rounded_md()
                                        .border_1()
                                        .border_color(rgb(0x45475a))
                                        .when(!busy, |button| {
                                            button
                                                .cursor_pointer()
                                                .hover(|hovered| hovered.bg(rgb(0x313244)))
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    cx.stop_propagation();
                                                    this.resource_dialog = None;
                                                    cx.notify();
                                                }))
                                        })
                                        .child("Cancel"),
                                )
                                .child(
                                    div()
                                        .id("resource-dialog-confirm")
                                        .h(px(34.0))
                                        .px_4()
                                        .flex()
                                        .items_center()
                                        .rounded_md()
                                        .bg(rgb(if dialog.kind == ResourceDialogKind::Remove {
                                            0x89384c
                                        } else {
                                            0x365a8c
                                        }))
                                        .when(!busy, |button| {
                                            button
                                                .cursor_pointer()
                                                .hover(|hovered| hovered.opacity(0.85))
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    cx.stop_propagation();
                                                    this.submit_resource_dialog(window, cx);
                                                }))
                                        })
                                        .child(confirm_label),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    fn help_overlay(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if !self.help_open {
            return None;
        }
        let sections = if self.navigation_region == NavigationRegion::Sidebar {
            [
                ShortcutSection::Sidebar,
                ShortcutSection::General,
                ShortcutSection::Navigation,
                ShortcutSection::Panes,
                ShortcutSection::Terminal,
            ]
        } else {
            [
                ShortcutSection::General,
                ShortcutSection::Navigation,
                ShortcutSection::Panes,
                ShortcutSection::Terminal,
                ShortcutSection::Sidebar,
            ]
        };
        let section_elements = sections.into_iter().map(|section| {
            let rows = HELP_SHORTCUTS
                .iter()
                .filter(move |shortcut| shortcut.section == section)
                .map(|shortcut| {
                    div()
                        .min_h(px(34.0))
                        .py_1()
                        .flex()
                        .items_center()
                        .gap_4()
                        .child(
                            div().w(px(190.0)).flex_none().child(
                                div()
                                    .flex()
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(rgb(0x45475a))
                                    .bg(rgb(0x11111b))
                                    .text_xs()
                                    .text_color(rgb(0xcdd6f4))
                                    .child(shortcut.keys),
                            ),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .text_sm()
                                .text_color(rgb(0xa6adc8))
                                .child(shortcut.description),
                        )
                })
                .collect::<Vec<_>>();
            div()
                .mb_4()
                .child(
                    div()
                        .mb_2()
                        .text_xs()
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(rgb(0x89b4fa))
                        .child(section.label()),
                )
                .children(rows)
        });

        Some(
            div()
                .id("help-backdrop")
                .absolute()
                .occlude()
                .left_0()
                .top_0()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(rgba(0x000000aa))
                .child(
                    div()
                        .id("help-dialog")
                        .w(px(720.0))
                        .h(relative(0.84))
                        .flex()
                        .flex_col()
                        .overflow_hidden()
                        .rounded_lg()
                        .border_1()
                        .border_color(rgb(0x45475a))
                        .bg(rgb(0x1e1e2e))
                        .shadow_lg()
                        .child(
                            div()
                                .h(px(64.0))
                                .flex_none()
                                .px_5()
                                .flex()
                                .items_center()
                                .justify_between()
                                .border_b_1()
                                .border_color(rgb(0x313244))
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .child(
                                            div()
                                                .text_lg()
                                                .font_weight(gpui::FontWeight::BOLD)
                                                .child("Keyboard shortcuts"),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(rgb(0x7f849c))
                                                .child("Boomux Desktop"),
                                        ),
                                )
                                .child(
                                    div()
                                        .id("help-close")
                                        .size(px(32.0))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded_md()
                                        .cursor_pointer()
                                        .text_color(rgb(0xa6adc8))
                                        .hover(|button| button.bg(rgb(0x313244)))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            cx.stop_propagation();
                                            this.help_open = false;
                                            cx.notify();
                                        }))
                                        .child("×"),
                                ),
                        )
                        .child(
                            div()
                                .id("help-scroll")
                                .track_scroll(&self.help_scroll_handle)
                                .flex_1()
                                .min_h_0()
                                .overflow_y_scroll()
                                .px_5()
                                .py_4()
                                .children(section_elements),
                        )
                        .child(
                            div()
                                .h(px(38.0))
                                .flex_none()
                                .px_5()
                                .flex()
                                .items_center()
                                .justify_between()
                                .border_t_1()
                                .border_color(rgb(0x313244))
                                .text_xs()
                                .text_color(rgb(0x7f849c))
                                .child("↑/↓ or J/K · Page Up/Down to scroll")
                                .child("Esc or F1 to close"),
                        ),
                )
                .into_any_element(),
        )
    }

    fn sidebar_agent(
        &self,
        agent: AgentChoice,
        focused_shell_id: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let shell_id = agent.shell_id.clone();
        let agent_item = SidebarItem::Agent {
            agent_id: agent.id.clone(),
            shell_id: agent.shell_id.clone(),
        };
        let keyboard_selected = self.navigation_region == NavigationRegion::Sidebar
            && self.sidebar_item.as_ref() == Some(&agent_item);
        let selected = focused_shell_id == Some(agent.shell_id.as_str());
        let state = agent.state_label();
        let completed = agent.completed_attention || self.completed_agents.contains(&agent.id);
        let dismissible = completed || agent.needs_attention;
        let dismissing = self.dismissing_agents.contains(&agent.id);
        let dismiss_agent_id = agent.id.clone();
        let dismiss_element_id = agent.id.clone();
        let attention_revision = agent.attention_revision;
        let glyph = if agent.needs_attention {
            "!"
        } else if state == "working" {
            "●"
        } else if completed || state == "finished" {
            "✓"
        } else {
            "○"
        };
        let glyph_color = if agent.needs_attention {
            0xf38ba8
        } else if selected || completed || matches!(state, "working" | "finished") {
            0x89b4fa
        } else {
            0x6c7086
        };
        div()
            .id(SharedString::from(format!("sidebar-agent-{}", agent.id)))
            .h(px(56.0))
            .flex_none()
            .overflow_hidden()
            .px_2()
            .flex()
            .items_center()
            .gap_2()
            .rounded_md()
            .anchor_scroll(keyboard_selected.then(|| self.sidebar_agent_scroll_anchor.clone()))
            .bg(if keyboard_selected {
                rgb(0x45475a)
            } else if selected {
                rgb(0x25283c)
            } else {
                rgb(0x181825)
            })
            .hover(|element| element.bg(rgb(0x29293d)))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, window, cx| {
                this.activate_sidebar_shell(&shell_id, window, cx);
            }))
            .child(
                div()
                    .w(px(14.0))
                    .flex_none()
                    .text_color(rgb(glyph_color))
                    .child(glyph),
            )
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .w_full()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_sm()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(rgb(0xcdd6f4))
                                    .child(agent.display_name),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .whitespace_nowrap()
                                    .text_xs()
                                    .text_color(rgb(0x6c7086))
                                    .child(relative_time(agent.updated_at_ms)),
                            ),
                    )
                    .child(
                        div()
                            .w_full()
                            .truncate()
                            .text_xs()
                            .text_color(if agent.needs_attention {
                                rgb(0xf38ba8)
                            } else {
                                rgb(0x7f849c)
                            })
                            .child(format!(
                                "{} · {} · {}",
                                if completed { "finished" } else { state },
                                agent.workspace,
                                agent.integration
                            )),
                    ),
            )
            .when(dismissible, |row| {
                row.child(
                    div()
                        .id(SharedString::from(format!(
                            "dismiss-agent-{dismiss_element_id}"
                        )))
                        .flex_none()
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .border_1()
                        .border_color(rgb(0x6c7086))
                        .text_xs()
                        .text_color(rgb(0xcdd6f4))
                        .cursor_pointer()
                        .hover(|element| element.bg(rgb(0x313244)))
                        .child(if dismissing { "…" } else { "Dismiss" })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.dismiss_agent_notification(
                                dismiss_agent_id.clone(),
                                attention_revision,
                                cx,
                            );
                        })),
                )
            })
    }

    fn boomux_body(&self, pane_id: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let pane = self.terminals.get(&pane_id);
        if pane.and_then(|pane| pane.session.as_ref()).is_some()
            && let Some(screen) = pane.and_then(|pane| pane.screen.as_ref())
        {
            let maximum = screen.scroll_total.saturating_sub(screen.scroll_len);
            let estimated_track_height =
                f32::from(screen.rows) * TERMINAL_CELL_HEIGHT + TERMINAL_PADDING;
            let thumb_fraction = scrollbar_thumb_fraction(screen, estimated_track_height);
            let progress = if maximum == 0 {
                1.0
            } else {
                (screen.scroll_offset as f32 / maximum as f32).clamp(0.0, 1.0)
            };
            let thumb_top = progress * (1.0 - thumb_fraction);
            let scrollbar_hovered = pane.is_some_and(|pane| pane.scrollbar_hovered);
            let scrollbar_dragging = self
                .terminal_scrollbar_drag
                .as_ref()
                .is_some_and(|drag| drag.pane_id == pane_id);
            let scrollbar_visible = scrollbar_hovered || scrollbar_dragging;
            let fade_generation = pane.map_or(0, |pane| pane.scrollbar_fade_generation);
            let scrollbar_visual = div()
                .absolute()
                .size_full()
                .rounded_full()
                .bg(rgb(0x1e1e2e))
                .child(
                    div()
                        .absolute()
                        .left(px(2.0))
                        .right(px(2.0))
                        .top(relative(thumb_top))
                        .h(relative(thumb_fraction))
                        .rounded_full()
                        .bg(rgb(if maximum == 0 { 0x313244 } else { 0x6c7086 })),
                );
            let scrollbar_visual = if fade_generation == 0 {
                scrollbar_visual
                    .opacity(if scrollbar_visible { 1.0 } else { 0.0 })
                    .into_any_element()
            } else {
                let animation_id = SharedString::from(format!(
                    "terminal-scrollbar-fade-{pane_id}-{fade_generation}"
                ));
                let duration = if scrollbar_visible {
                    SCROLLBAR_FADE_IN_DURATION
                } else {
                    SCROLLBAR_FADE_OUT_DURATION
                };
                scrollbar_visual
                    .with_animation(
                        animation_id,
                        Animation::new(duration),
                        move |element, progress| {
                            element.opacity(scrollbar_fade_opacity(scrollbar_visible, progress))
                        },
                    )
                    .into_any_element()
            };
            let scrollbar = div()
                .id(("terminal-scrollbar", pane_id))
                .absolute()
                .right(px(1.0))
                .top(px(2.0))
                .bottom(px(2.0))
                .w(px(10.0))
                .cursor(CursorStyle::Arrow)
                .on_hover(cx.listener(move |this, hovered, _, cx| {
                    this.set_terminal_scrollbar_hover(pane_id, *hovered, cx);
                }))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event, window, cx| {
                        this.begin_terminal_scrollbar_drag(pane_id, event, window, cx);
                    }),
                )
                .child(scrollbar_visual);
            return div()
                .relative()
                .size_full()
                .child(terminal_view(
                    Arc::clone(
                        pane.and_then(|pane| pane.paint_cache.as_ref())
                            .expect("terminal paint cache refreshed before rendering"),
                    ),
                    screen
                        .image_placements
                        .iter()
                        .filter_map(|placement| {
                            pane.and_then(|pane| {
                                pane.render_images.get(&placement.image_generation).cloned()
                            })
                            .map(|image| (placement.clone(), image))
                        })
                        .collect(),
                ))
                .child(scrollbar)
                .into_any_element();
        }

        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .overflow_hidden()
            .child(div().text_xs().text_color(rgb(0xa6adc8)).child(
                if pane.is_some_and(|pane| pane.attaching) {
                    "Opening terminal…"
                } else {
                    "No Boomux terminal is available."
                },
            ))
            .when(
                pane.is_some_and(|pane| {
                    pane.restored
                        .as_ref()
                        .is_some_and(|saved| saved.shell.is_some())
                        && !pane.attaching
                        && pane.session.is_none()
                }),
                |element| {
                    element.child(
                        Self::settings_option(
                            ("reconnect-saved-pane", pane_id),
                            "Reconnect / start Shell",
                            false,
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.reconnect_saved_pane(pane_id, window, cx)
                            },
                        )),
                    )
                },
            )
            .when_some(
                pane.and_then(|pane| pane.error.clone())
                    .or_else(|| self.boomux_error.clone()),
                |element, error| {
                    element.child(div().text_xs().text_color(rgb(0xf38ba8)).child(error))
                },
            )
            .into_any_element()
    }

    fn pane_resize_handles(&self, id: usize, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        if self.fullscreen == Some(id) || self.workspace_transition.is_some() {
            return Vec::new();
        }
        let floating = self.floating.iter().any(|pane| pane.id == id);
        let mut handles = [
            Direction::Left,
            Direction::Right,
            Direction::Up,
            Direction::Down,
        ]
        .into_iter()
        .enumerate()
        .filter_map(|(index, edge)| {
            if !floating
                && !self
                    .layout
                    .as_ref()
                    .is_some_and(|layout| layout.has_resize_edge(id, edge))
            {
                return None;
            }
            let handle = div()
                .id(SharedString::from(format!("pane-resize-{id}-{index}")))
                .absolute()
                .occlude()
                .hover(|handle| handle.bg(rgb(0x89b4fa)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event, window, cx| {
                        this.begin_pointer_interaction_focus(id, window, cx);
                        this.start_pointer_drag(id, PointerOperation::ResizeEdge(edge), event, cx);
                    }),
                )
                .on_mouse_move(cx.listener(Self::on_pointer_move))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(Self::end_pointer_interaction),
                );
            Some(
                match edge {
                    Direction::Left => handle
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(px(4.0))
                        .cursor(gpui::CursorStyle::ResizeLeftRight),
                    Direction::Right => handle
                        .right_0()
                        .top_0()
                        .bottom_0()
                        .w(px(4.0))
                        .cursor(gpui::CursorStyle::ResizeLeftRight),
                    Direction::Up => handle
                        .top_0()
                        .left_0()
                        .right_0()
                        .h(px(4.0))
                        .cursor(gpui::CursorStyle::ResizeUpDown),
                    Direction::Down => handle
                        .bottom_0()
                        .left_0()
                        .right_0()
                        .h(px(4.0))
                        .cursor(gpui::CursorStyle::ResizeUpDown),
                }
                .into_any_element(),
            )
        })
        .collect::<Vec<_>>();
        // Paint corner hitboxes after side handles so diagonal dragging wins
        // where the two side targets meet.
        for (index, (horizontal, vertical)) in [
            (Direction::Left, Direction::Up),
            (Direction::Right, Direction::Up),
            (Direction::Left, Direction::Down),
            (Direction::Right, Direction::Down),
        ]
        .into_iter()
        .enumerate()
        {
            if !floating
                && !self.layout.as_ref().is_some_and(|layout| {
                    layout.has_resize_edge(id, horizontal) && layout.has_resize_edge(id, vertical)
                })
            {
                continue;
            }
            let handle = div()
                .id(SharedString::from(format!(
                    "pane-corner-resize-{id}-{index}"
                )))
                .absolute()
                .size(px(12.0))
                .occlude()
                .cursor(
                    if matches!(
                        (horizontal, vertical),
                        (Direction::Left, Direction::Up) | (Direction::Right, Direction::Down)
                    ) {
                        gpui::CursorStyle::ResizeUpLeftDownRight
                    } else {
                        gpui::CursorStyle::ResizeUpRightDownLeft
                    },
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event, window, cx| {
                        this.begin_pointer_interaction_focus(id, window, cx);
                        this.start_pointer_drag(
                            id,
                            PointerOperation::ResizeCorner(horizontal, vertical),
                            event,
                            cx,
                        );
                    }),
                )
                .on_mouse_move(cx.listener(Self::on_pointer_move))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(Self::end_pointer_interaction),
                );
            let handle = if horizontal == Direction::Left {
                handle.left_0()
            } else {
                handle.right_0()
            };
            let handle = if vertical == Direction::Up {
                handle.top_0()
            } else {
                handle.bottom_0()
            };
            handles.push(handle.into_any_element());
        }
        handles
    }

    fn shell_has_agent(&self, shell: &ShellChoice) -> bool {
        self.boomux_overview.agents.iter().any(|agent| {
            agent.shell_id == shell.id
                && shell.run_id.as_deref() == Some(agent.run_id.as_str())
                && !matches!(agent.state, AgentState::Inactive | AgentState::Done)
        })
    }

    fn pane(&self, id: usize, cx: &mut Context<Self>) -> Stateful<Div> {
        self.pane_with_heading(id, self.pane_headings_visible, cx)
    }

    fn pane_with_heading(
        &self,
        id: usize,
        show_heading: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let focused = self.focused == id;
        let maximized = self.fullscreen == Some(id);
        let floating = self.floating.iter().any(|pane| pane.id == id);
        let pane = self.terminals.get(&id);
        let title: SharedString = pane.and_then(|pane| pane.session.as_ref()).map_or_else(
            || "Boomux terminals".into(),
            |terminal| terminal.shell_name.clone().into(),
        );
        let accent = rgb(0xa6e3a1);
        let is_agent = pane
            .and_then(|pane| pane.shell.as_ref())
            .is_some_and(|shell| self.shell_has_agent(shell));
        let corners = pane_corner_radii(id, self.pane_corner_style);
        let focused_border = blend_rgb(
            theme::resolve_legacy(0x313244),
            theme::resolve_legacy(0xcba6f7),
            self.focus_highlight_strength,
        );
        let focused_heading = blend_rgb(
            theme::resolve_legacy(0x1e1e2e),
            theme::resolve_legacy(0x313244),
            self.focus_highlight_strength,
        );
        let status_message = pane
            .and_then(|pane| pane.session.as_ref())
            .and_then(TerminalSession::status_message);

        div()
            .id(("pane", id))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded_tl(px(corners[0]))
            .rounded_tr(px(corners[1]))
            .rounded_br(px(corners[2]))
            .rounded_bl(px(corners[3]))
            .border_2()
            .border_color(if focused {
                gpui::rgb(focused_border)
            } else {
                rgb(0x313244)
            })
            .bg(rgb(0x181825))
            .shadow_lg()
            .on_mouse_move(cx.listener(move |this, event, window, cx| {
                this.focus_pane_on_hover(id, event, window, cx);
            }))
            .on_scroll_wheel(cx.listener(move |this, event, window, cx| {
                this.scroll_terminal(id, event, window, cx);
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event, window, cx| {
                    this.begin_pointer_interaction(id, event, window, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event, window, cx| {
                    this.begin_pointer_interaction(id, event, window, cx);
                }),
            )
            .when(show_heading, |element| {
                element.child(
                    div()
                        .h(px(38.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_between()
                        .px_3()
                        .cursor_grab()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, event, window, cx| {
                                this.begin_heading_drag(id, event, window, cx);
                            }),
                        )
                        .bg(if focused {
                            gpui::rgb(focused_heading)
                        } else {
                            rgb(0x1e1e2e)
                        })
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(if is_agent {
                                    div().flex_none().text_sm().text_color(accent).child("✦")
                                } else {
                                    div().size_2().flex_none().rounded_full().bg(accent)
                                })
                                .child(div().min_w_0().overflow_hidden().child(title))
                                .child(
                                    div()
                                        .id(("rename-pane", id))
                                        .w(px(22.0))
                                        .h(px(22.0))
                                        .flex_none()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded_md()
                                        .text_xs()
                                        .text_color(rgb(0x7f849c))
                                        .cursor_pointer()
                                        .hover(|button| {
                                            button.bg(rgb(0x45475a)).text_color(rgb(0xcdd6f4))
                                        })
                                        .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                            cx.stop_propagation();
                                        })
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            cx.stop_propagation();
                                            this.request_pane_shell_dialog(
                                                id,
                                                ResourceDialogKind::Rename,
                                                window,
                                                cx,
                                            );
                                        }))
                                        .child("✎"),
                                ),
                        )
                        .child(
                            div()
                                .flex_none()
                                .flex()
                                .items_center()
                                .gap_1()
                                .when_some(status_message, |controls, status| {
                                    controls.child(
                                        div()
                                            .mr_1()
                                            .text_xs()
                                            .text_color(rgb(0xf9e2af))
                                            .child(status),
                                    )
                                })
                                .child(
                                    div()
                                        .id(("float-pane", id))
                                        .w(px(28.0))
                                        .h(px(26.0))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded_md()
                                        .border_1()
                                        .border_color(rgb(0x45475a))
                                        .text_color(rgb(0xa6adc8))
                                        .cursor_pointer()
                                        .hover(|button| button.bg(rgb(0x45475a)))
                                        .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                            cx.stop_propagation();
                                        })
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            cx.stop_propagation();
                                            this.focus_terminal_pane(id, window, cx);
                                            this.toggle_floating(&ToggleFloating, window, cx);
                                        }))
                                        .child(if floating { "↙" } else { "↗" }),
                                )
                                .child(
                                    div()
                                        .id(("maximize-pane", id))
                                        .w(px(28.0))
                                        .h(px(26.0))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded_md()
                                        .border_1()
                                        .border_color(rgb(0x45475a))
                                        .text_color(rgb(0xa6adc8))
                                        .cursor_pointer()
                                        .hover(|button| button.bg(rgb(0x45475a)))
                                        .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                            cx.stop_propagation();
                                        })
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            cx.stop_propagation();
                                            this.focus_terminal_pane(id, window, cx);
                                            this.toggle_fullscreen(&ToggleFullscreen, window, cx);
                                        }))
                                        .child(if maximized { "❐" } else { "□" }),
                                )
                                .child(
                                    div()
                                        .id(("minimize-pane", id))
                                        .w(px(28.0))
                                        .h(px(26.0))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded_md()
                                        .border_1()
                                        .border_color(rgb(0x45475a))
                                        .text_color(rgb(0xa6adc8))
                                        .cursor_pointer()
                                        .hover(|button| button.bg(rgb(0x45475a)))
                                        .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                            cx.stop_propagation();
                                        })
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            cx.stop_propagation();
                                            this.minimize_pane(id, window, cx);
                                        }))
                                        .child("−"),
                                )
                                .child(
                                    div()
                                        .id(("close-pane", id))
                                        .w(px(28.0))
                                        .h(px(26.0))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded_md()
                                        .border_1()
                                        .border_color(rgb(0x45475a))
                                        .text_color(rgb(0xa6adc8))
                                        .cursor_pointer()
                                        .hover(|button| {
                                            button.bg(rgb(0x89384c)).text_color(rgb(0xf5e0e6))
                                        })
                                        .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                            cx.stop_propagation();
                                        })
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            cx.stop_propagation();
                                            this.request_pane_shell_dialog(
                                                id,
                                                ResourceDialogKind::Remove,
                                                window,
                                                cx,
                                            );
                                        }))
                                        .child("×"),
                                ),
                        ),
                )
            })
            .child(
                div()
                    .id(("terminal-interaction", id))
                    .relative()
                    .overflow_hidden()
                    .flex_1()
                    .min_h_0()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event, _, cx| {
                            this.begin_terminal_selection(id, event, cx);
                        }),
                    )
                    .on_drag(TerminalSelectionDrag { pane_id: id }, move |_, _, _, cx| {
                        cx.new(move |_| TerminalSelectionDrag { pane_id: id })
                    })
                    .on_drag_move(cx.listener(move |this, event, window, cx| {
                        this.drag_terminal_selection(id, event, window, cx);
                    }))
                    .on_mouse_down(MouseButton::Middle, cx.listener(Self::paste_primary))
                    .child(self.boomux_body(id, cx))
                    .when(self.copied_pane == Some(id), |body| {
                        body.child(
                            div()
                                .absolute()
                                .bottom(px(12.0))
                                .right(px(20.0))
                                .px_3()
                                .py_1()
                                .rounded_md()
                                .bg(rgb(0x313244))
                                .text_color(rgb(0xa6e3a1))
                                .text_sm()
                                .child("Copied"),
                        )
                    })
                    .when(
                        self.layout_overlay_visible
                            && (self.layout_mode
                                || (self.layout_badge_exiting
                                    && self.motion_speed.duration().is_some())),
                        |body| {
                            body.child(layout_badge::render_pane_overlay(
                                self.theme,
                                self.motion_speed.duration(),
                                !self.layout_mode,
                                self.layout_badge_generation,
                                (id, self.animation_generation),
                            ))
                        },
                    ),
            )
            .children(self.pane_resize_handles(id, cx))
    }

    fn minimized_tab_strip(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let shells = self.minimized_tab_shells();
        if shells.is_empty() {
            return None;
        }
        let tabs = shells
            .into_iter()
            .map(|shell| {
                let shell_id = shell.id.clone();
                let rename_target = SidebarResource::Shell {
                    id: shell.id.clone(),
                    workspace_id: shell.workspace_id.clone(),
                    name: shell.name.clone(),
                };
                div()
                    .id(SharedString::from(format!("minimized-tab-{}", shell.id)))
                    .h(px(32.0))
                    .w(px(196.0))
                    .flex_none()
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .overflow_hidden()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(0x313244))
                    .bg(rgb(0x1e1e2e))
                    .text_color(rgb(0xcdd6f4))
                    .cursor_pointer()
                    .hover(|tab| tab.bg(rgb(0x29293d)).border_color(rgb(0x585b70)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.activate_sidebar_shell(&shell_id, window, cx);
                    }))
                    .child(div().size_1().flex_none().rounded_full().bg(rgb(0x6c7086)))
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(shell.name),
                    )
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .id(SharedString::from(format!(
                                        "rename-minimized-tab-{}",
                                        shell.id
                                    )))
                                    .w(px(22.0))
                                    .h(px(22.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_md()
                                    .text_xs()
                                    .text_color(rgb(0x7f849c))
                                    .hover(|button| {
                                        button.bg(rgb(0x45475a)).text_color(rgb(0xcdd6f4))
                                    })
                                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation();
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.open_resource_dialog(
                                            ResourceDialogKind::Rename,
                                            rename_target.clone(),
                                        );
                                        cx.notify();
                                    }))
                                    .child("✎"),
                            )
                            .child(div().text_xs().text_color(rgb(0x7f849c)).child("restore")),
                    )
            })
            .collect::<Vec<_>>();

        Some(
            div()
                .id("minimized-tab-strip")
                .h(px(TAB_BAR_HEIGHT))
                .flex_none()
                .flex()
                .items_center()
                .border_b_1()
                .border_color(rgb(0x313244))
                .bg(rgb(0x11111b))
                .child(
                    div()
                        .id("minimized-tabs-previous")
                        .w(px(28.0))
                        .h_full()
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .border_r_1()
                        .border_color(rgb(0x313244))
                        .text_color(rgb(0xa6adc8))
                        .cursor_pointer()
                        .hover(|button| button.bg(rgb(0x29293d)))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.scroll_minimized_tabs(-1, cx);
                        }))
                        .child("‹"),
                )
                .child(
                    div()
                        .id("minimized-terminal-tabs")
                        .min_w_0()
                        .h_full()
                        .flex_1()
                        .flex()
                        .items_center()
                        .gap_1()
                        .px_1()
                        .overflow_x_scroll()
                        .track_scroll(&self.minimized_tab_scroll_handle)
                        .children(tabs),
                )
                .child(
                    div()
                        .id("minimized-tabs-next")
                        .w(px(28.0))
                        .h_full()
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .border_l_1()
                        .border_color(rgb(0x313244))
                        .text_color(rgb(0xa6adc8))
                        .cursor_pointer()
                        .hover(|button| button.bg(rgb(0x29293d)))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.scroll_minimized_tabs(1, cx);
                        }))
                        .child("›"),
                )
                .into_any_element(),
        )
    }

    fn render_layout(&self, layout: &Node, cx: &mut Context<Self>) -> gpui::AnyElement {
        let animation_duration = self.motion_speed.duration();
        let mut rects = workspace_layout_rects(layout, self.fullscreen);
        let paint_last = self.fullscreen.or_else(|| {
            self.layout_animation
                .as_ref()
                .and_then(|animation| animation.paint_last)
        });
        paint_layout_pane_last(&mut rects, paint_last);
        let panes = rects
            .into_iter()
            .map(|(id, target)| {
                let pane = self.pane(id, cx);
                let base = div().absolute().p(px(self.pane_gap / 2.0)).child(pane);
                if let (Some(animation), Some(duration)) =
                    (&self.layout_animation, animation_duration)
                {
                    let from = animation.from.get(&id).copied().unwrap_or(target);
                    let animation_id =
                        SharedString::from(format!("layout-reflow-{}-{id}", animation.generation));
                    base.with_animation(
                        animation_id,
                        Animation::new(duration).with_easing(ease_out_quint()),
                        move |element, progress| {
                            let rect = interpolate_rect(from, target, progress);
                            element
                                .left(relative(rect.x))
                                .top(relative(rect.y))
                                .w(relative(rect.width))
                                .h(relative(rect.height))
                        },
                    )
                    .into_any_element()
                } else {
                    base.left(relative(target.x))
                        .top(relative(target.y))
                        .w(relative(target.width))
                        .h(relative(target.height))
                        .into_any_element()
                }
            })
            .collect::<Vec<_>>();
        div()
            .relative()
            .size_full()
            .children(panes)
            .into_any_element()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sidebar_viewport_width = f32::from(window.viewport_size().width);
        self.layout_canvas = self.panel_size(window);
        let workspace_name = self
            .terminals
            .get(&self.focused)
            .and_then(|pane| pane.shell.as_ref())
            .and_then(|shell| {
                self.boomux_overview
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.id == shell.workspace_id)
            })
            .map(|workspace| workspace.name.as_str());
        let desktop_title = desktop_window_title(workspace_name);
        if window.window_title() != desktop_title {
            window.set_window_title(&desktop_title);
        }
        for (&id, pane) in &self.terminals {
            if self
                .minimizing_panes
                .iter()
                .any(|animation| animation.pane_id == id)
                || self
                    .workspace_transition
                    .as_ref()
                    .is_some_and(|transition| {
                        transition.outgoing.iter().any(|outgoing| outgoing.id == id)
                    })
            {
                continue;
            }
            if let Some(terminal) = pane.session.as_ref() {
                let (rows, cols, pixel_width, pixel_height) = self.terminal_grid_size(id, window);
                terminal.resize(rows, cols, pixel_width, pixel_height);
            }
        }
        self.refresh_terminal_images(window);
        self.refresh_terminal_paint_caches(window);
        let tiled = if let Some(layout) = &self.layout {
            self.render_layout(layout, cx)
        } else if self.boomux_overview.workspaces.is_empty() && self.terminals.is_empty() {
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_3()
                .p_6()
                .text_center()
                .text_sm()
                .text_color(rgb(0xa6adc8))
                .child(
                    div()
                        .text_xl()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(rgb(0xcdd6f4))
                        .child("Create your first Workspace"),
                )
                .child("Click + in the sidebar, then choose New Workspace to get started.")
                .child(if cfg!(target_os = "macos") {
                    "Once created, press Command + Enter to add a Shell."
                } else {
                    "Once created, press Ctrl + Enter to add a Shell."
                })
                .child("Press F1 for keyboard shortcuts.")
                .into_any_element()
        } else {
            div().size_full().into_any_element()
        };
        let lifted_id = match &self.pointer_drag {
            Some(PointerDrag {
                pane_id,
                subject: PointerSubject::Lifted(_),
                ..
            }) => Some(*pane_id),
            _ => None,
        };
        let floating_animation = self.floating_animation.clone();
        let motion_duration = self.motion_speed.duration();
        let mut floating_panes = self.floating.clone();
        if let Some(id) = self.fullscreen {
            if self
                .layout
                .as_ref()
                .is_some_and(|layout| layout.contains(id))
            {
                // A maximized tiled pane covers the floating layer as well.
                floating_panes.clear();
            } else {
                floating_panes.sort_by_key(|pane| pane.id == id);
            }
        }
        let floating = floating_panes
            .into_iter()
            .map(|stored_pane| {
                let pane = if self.fullscreen == Some(stored_pane.id) {
                    workspace_maximized_pane(stored_pane.id, self.panel_size(window), self.pane_gap)
                } else {
                    stored_pane
                };
                let base = div()
                    .absolute()
                    // Floating panes paint above the tiled layout and must
                    // also own the corresponding pointer hitbox. Without
                    // occlusion, both overlapping pane hover handlers run
                    // and the tiled pane behind wins focus last.
                    .occlude()
                    // An occluding hitbox also prevents the workspace's
                    // behind-the-pane move listener from receiving pointer
                    // events. Track an active drag on the floating layer so
                    // every pointer sample updates its geometry smoothly.
                    .on_mouse_move(cx.listener(Self::on_pointer_move))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(Self::end_pointer_interaction),
                    )
                    .on_mouse_up(
                        MouseButton::Right,
                        cx.listener(Self::end_pointer_interaction),
                    )
                    .when(lifted_id == Some(pane.id), |element| element.opacity(0.92))
                    .child(self.pane(pane.id, cx));
                if let Some(transition) = &self.workspace_transition {
                    let target = pane.clone();
                    let from = FloatingPane {
                        x: target.x + transition.direction * self.panel_size(window).0,
                        ..target.clone()
                    };
                    let animation_id = SharedString::from(format!(
                        "workspace-enter-{}-{}",
                        transition.generation, pane.id
                    ));
                    base.with_animation(
                        animation_id,
                        Animation::new(transition.duration).with_easing(ease_out_quint()),
                        move |element, progress| {
                            let bounds = interpolate_floating_pane(&from, &target, progress);
                            element
                                .left(px(bounds.x))
                                .top(px(bounds.y))
                                .w(px(bounds.width))
                                .h(px(bounds.height))
                        },
                    )
                    .into_any_element()
                } else if let (Some(animation), Some(duration)) = (
                    floating_animation
                        .as_ref()
                        .filter(|animation| animation.pane_id == pane.id),
                    motion_duration,
                ) {
                    let from = animation.from.clone();
                    let target = pane.clone();
                    let animation_id = SharedString::from(format!(
                        "floating-transition-{}-{}",
                        animation.generation, pane.id
                    ));
                    base.with_animation(
                        animation_id,
                        Animation::new(duration).with_easing(ease_out_quint()),
                        move |element, progress| {
                            let bounds = interpolate_floating_pane(&from, &target, progress);
                            element
                                .left(px(bounds.x))
                                .top(px(bounds.y))
                                .w(px(bounds.width))
                                .h(px(bounds.height))
                        },
                    )
                    .into_any_element()
                } else {
                    base.left(px(pane.x))
                        .top(px(pane.y))
                        .w(px(pane.width))
                        .h(px(pane.height))
                        .into_any_element()
                }
            })
            .collect::<Vec<_>>();

        let minimizing = self
            .minimizing_panes
            .iter()
            .map(|animation| {
                let from = animation.from.clone();
                let target = FloatingPane {
                    id: animation.pane_id,
                    x: from.x + from.width / 2.0 - 36.0,
                    y: 0.0,
                    width: 72.0,
                    height: 24.0,
                };
                let animation_id = SharedString::from(format!(
                    "pane-minimize-{}-{}",
                    animation.generation, animation.pane_id
                ));
                div()
                    .absolute()
                    .occlude()
                    .child(self.pane(animation.pane_id, cx))
                    .with_animation(
                        animation_id,
                        Animation::new(animation.duration).with_easing(ease_out_quint()),
                        move |element, progress| {
                            let bounds = interpolate_floating_pane(&from, &target, progress);
                            element
                                .left(px(bounds.x))
                                .top(px(bounds.y))
                                .w(px(bounds.width))
                                .h(px(bounds.height))
                                .opacity(1.0 - progress)
                        },
                    )
                    .into_any_element()
            })
            .collect::<Vec<_>>();

        let workspace_leaving = self
            .workspace_transition
            .as_ref()
            .map(|transition| {
                let panel_width = self.panel_size(window).0;
                transition
                    .outgoing
                    .iter()
                    .filter(|outgoing| self.terminals.contains_key(&outgoing.id))
                    .map(|outgoing| {
                        let from = outgoing.clone();
                        let target = FloatingPane {
                            x: from.x - transition.direction * panel_width,
                            ..from.clone()
                        };
                        let animation_id = SharedString::from(format!(
                            "workspace-leave-{}-{}",
                            transition.generation, outgoing.id
                        ));
                        div()
                            .absolute()
                            .p(px(self.pane_gap / 2.0))
                            .child(self.pane(outgoing.id, cx))
                            .with_animation(
                                animation_id,
                                Animation::new(transition.duration).with_easing(ease_out_quint()),
                                move |element, progress| {
                                    let bounds =
                                        interpolate_floating_pane(&from, &target, progress);
                                    element
                                        .left(px(bounds.x))
                                        .top(px(bounds.y))
                                        .w(px(bounds.width))
                                        .h(px(bounds.height))
                                },
                            )
                            .into_any_element()
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let minimized_tabs = self.minimized_tab_strip(cx);
        let layout_mode_indicator = (self.layout_mode
            || (self.layout_badge_exiting && self.motion_speed.duration().is_some()))
        .then(|| {
            layout_badge::render(
                self.theme,
                self.motion_speed.duration(),
                !self.layout_mode,
                self.layout_badge_generation,
                (self.focused, self.animation_generation),
            )
        });
        let terminal_area = div()
            .h_full()
            .min_w_0()
            .flex_1()
            .flex()
            .flex_col()
            .when_some(minimized_tabs, |element, tabs| element.child(tabs))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .p(px(self.pane_gap))
                    .child(tiled)
                    .children(floating)
                    .children(minimizing)
                    .children(workspace_leaving)
                    .when_some(layout_mode_indicator, |element, indicator| {
                        element.child(indicator)
                    }),
            );

        let target_drawer_width = self.sidebar_width();
        let sidebar = div()
            .relative()
            .h_full()
            .w(px(target_drawer_width))
            .child(self.sidebar(window, cx))
            .when(target_drawer_width > 0.0, |sidebar| {
                sidebar.child(
                    div()
                        .id("sidebar-resize")
                        .absolute()
                        .right_0()
                        .top_0()
                        .bottom_0()
                        .w(px(5.0))
                        .occlude()
                        .cursor(gpui::CursorStyle::ResizeLeftRight)
                        .hover(|handle| handle.bg(rgb(0x89b4fa)))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                this.sidebar_resizing = true;
                                this.drawer_animation_from = None;
                                this.git_panel.search_focused = false;
                                cx.stop_propagation();
                            }),
                        )
                        .on_mouse_move(cx.listener(Self::on_pointer_move))
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(Self::end_pointer_interaction),
                        ),
                )
            });
        let drawer = if let Some(from) = self.drawer_animation_from {
            let animation_id = SharedString::from(format!(
                "sidebar-drawer-{}",
                self.drawer_animation_generation
            ));
            div()
                .h_full()
                .flex_none()
                .overflow_hidden()
                .child(sidebar)
                .with_animation(
                    animation_id,
                    Animation::new(DRAWER_ANIMATION_DURATION).with_easing(ease_out_quint()),
                    move |element, progress| {
                        let width = from + (target_drawer_width - from) * progress;
                        element.w(px(width))
                    },
                )
                .into_any_element()
        } else {
            div()
                .h_full()
                .w(px(target_drawer_width))
                .flex_none()
                .overflow_hidden()
                .child(sidebar)
                .into_any_element()
        };

        let content = div()
            .relative()
            .size_full()
            .flex()
            .child(drawer)
            .child(terminal_area)
            .when(!self.sidebar_visible, |content| {
                content.child(
                    div()
                        .id("sidebar-reopen-handle")
                        .absolute()
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(px(6.0))
                        .occlude()
                        .cursor(gpui::CursorStyle::ResizeLeftRight)
                        .hover(|handle| handle.bg(rgb(0x89b4fa)))
                        .tooltip(|_, cx| {
                            cx.new(|_| HeaderTooltip("Drag right to open sidebar"))
                                .into()
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                this.sidebar_resizing = true;
                                this.drawer_animation_from = None;
                                cx.stop_propagation();
                            }),
                        )
                        .on_mouse_move(cx.listener(Self::on_pointer_move))
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(Self::end_pointer_interaction),
                        ),
                )
            })
            .into_any_element();
        let sidebar_menu = self.sidebar_menu_overlay(cx);
        let resource_dialog = self.resource_dialog_overlay(cx);
        let settings_restart = self.settings_restart_overlay(cx);
        let help = self.help_overlay(cx);

        div()
            .id("workspace")
            .when_some(self.layout_error.clone(), |element, error| {
                element.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .text_xs()
                        .bg(rgb(0x313244))
                        .text_color(rgb(0xf38ba8))
                        .child(format!("Layout not saved: {error}")),
                )
            })
            .track_focus(&self.focus_handle)
            .key_context(
                if (self.nodes_open && self.navigation_region == NavigationRegion::Sidebar)
                    || self.git_panel.search_focused
                    || self.boomux_setting_input.is_some()
                    || self.settings_restart_confirm
                {
                    "BoomuxSettingsInput"
                } else {
                    workspace_key_context(self.help_open, self.navigation_region, self.layout_mode)
                },
            )
            .on_action(cx.listener(Self::focus_left))
            .on_action(cx.listener(Self::focus_right))
            .on_action(cx.listener(Self::focus_up))
            .on_action(cx.listener(Self::focus_down))
            .on_action(cx.listener(Self::move_left))
            .on_action(cx.listener(Self::move_right))
            .on_action(cx.listener(Self::move_up))
            .on_action(cx.listener(Self::move_down))
            .on_action(cx.listener(Self::resize_left))
            .on_action(cx.listener(Self::resize_right))
            .on_action(cx.listener(Self::resize_up))
            .on_action(cx.listener(Self::resize_down))
            .on_action(cx.listener(Self::resize_small_left))
            .on_action(cx.listener(Self::resize_small_right))
            .on_action(cx.listener(Self::resize_small_up))
            .on_action(cx.listener(Self::resize_small_down))
            .on_action(cx.listener(Self::resize_large_left))
            .on_action(cx.listener(Self::resize_large_right))
            .on_action(cx.listener(Self::resize_large_up))
            .on_action(cx.listener(Self::resize_large_down))
            .on_action(cx.listener(Self::toggle_split))
            .on_action(cx.listener(Self::equalize_split))
            .on_action(cx.listener(Self::swap_split))
            .on_action(cx.listener(Self::align_floating_left))
            .on_action(cx.listener(Self::align_floating_right))
            .on_action(cx.listener(Self::align_floating_up))
            .on_action(cx.listener(Self::align_floating_down))
            .on_action(cx.listener(Self::center_floating))
            .on_action(cx.listener(Self::cycle_pane_next))
            .on_action(cx.listener(Self::cycle_pane_previous))
            .on_action(cx.listener(Self::cycle_workspace_next))
            .on_action(cx.listener(Self::cycle_workspace_previous))
            .on_action(cx.listener(Self::new_pane))
            .on_action(cx.listener(Self::close_pane))
            .on_action(cx.listener(Self::toggle_floating))
            .on_action(cx.listener(Self::toggle_fullscreen))
            .on_action(cx.listener(Self::toggle_sidebar_drawer))
            .on_action(cx.listener(Self::toggle_sidebar_focus))
            .on_action(cx.listener(Self::toggle_help))
            .on_action(cx.listener(|this, _: &ToggleGitPanel, _, cx| this.toggle_git_panel(cx)))
            .on_action(cx.listener(Self::rename_resource))
            .on_action(cx.listener(Self::remove_shell))
            .on_action(cx.listener(Self::copy_selection))
            .on_action(cx.listener(Self::paste_clipboard))
            .on_action(cx.listener(Self::toggle_layout_mode))
            .on_action(cx.listener(Self::exit_layout_mode))
            .on_key_down(cx.listener(Self::terminal_key_down))
            .on_key_up(cx.listener(Self::terminal_key_up))
            .on_modifiers_changed(cx.listener(
                |this, event: &gpui::ModifiersChangedEvent, _, cx| {
                    if !event.modifiers.control {
                        this.release_layout_leader(cx);
                    }
                },
            ))
            .on_mouse_move(cx.listener(Self::on_pointer_move))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(Self::end_pointer_interaction),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(Self::end_pointer_interaction),
            )
            .on_mouse_up(
                MouseButton::Right,
                cx.listener(Self::end_pointer_interaction),
            )
            .on_mouse_up_out(
                MouseButton::Right,
                cx.listener(Self::end_pointer_interaction),
            )
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x11111b))
            .text_color(rgb(0xcdd6f4))
            .child(content)
            .when_some(sidebar_menu, |element, menu| element.child(menu))
            .when_some(resource_dialog, |element, dialog| element.child(dialog))
            .when_some(settings_restart, |element, dialog| element.child(dialog))
            .when_some(help, |element, help| element.child(help))
    }
}

fn desktop_keystroke(keystroke: &gpui::Keystroke, layout_mode: bool) -> bool {
    let modifiers = keystroke.modifiers;
    let no_modifiers = !modifiers.control
        && !modifiers.alt
        && !modifiers.shift
        && !modifiers.platform
        && !modifiers.function;
    let only_shift = modifiers.shift
        && !modifiers.control
        && !modifiers.alt
        && !modifiers.platform
        && !modifiers.function;
    let only_alt = modifiers.alt
        && !modifiers.control
        && !modifiers.shift
        && !modifiers.platform
        && !modifiers.function;
    let alt_shift = modifiers.alt
        && modifiers.shift
        && !modifiers.control
        && !modifiers.platform
        && !modifiers.function;
    let only_control = modifiers.control
        && !modifiers.alt
        && !modifiers.shift
        && !modifiers.platform
        && !modifiers.function;
    let only_secondary = modifiers.secondary()
        && !modifiers.alt
        && !modifiers.shift
        && !modifiers.function
        && if cfg!(target_os = "macos") {
            !modifiers.control
        } else {
            !modifiers.platform
        };
    let secondary_shift = modifiers.secondary()
        && modifiers.shift
        && !modifiers.alt
        && !modifiers.function
        && if cfg!(target_os = "macos") {
            !modifiers.control
        } else {
            !modifiers.platform
        };

    if (no_modifiers && matches!(keystroke.key.as_str(), "f1" | "f2" | "f6"))
        || (only_control && keystroke.key == "space")
        || (only_secondary && matches!(keystroke.key.as_str(), "enter" | "w"))
        || (secondary_shift && matches!(keystroke.key.as_str(), "c" | "v" | "w"))
        || (only_control && keystroke.key == "insert")
        || (only_shift && keystroke.key == "insert")
    {
        return true;
    }

    layout_mode
        && ((no_modifiers
            && matches!(
                keystroke.key.as_str(),
                "h" | "j"
                    | "k"
                    | "l"
                    | "left"
                    | "right"
                    | "up"
                    | "down"
                    | "tab"
                    | "pageup"
                    | "pagedown"
                    | "s"
                    | "e"
                    | "r"
                    | "c"
                    | "o"
                    | "f"
                    | "b"
                    | "escape"
            ))
            || (only_shift
                && matches!(
                    keystroke.key.as_str(),
                    "h" | "j" | "k" | "l" | "left" | "right" | "up" | "down" | "tab"
                ))
            || ((only_alt || alt_shift)
                && matches!(
                    keystroke.key.as_str(),
                    "h" | "j" | "k" | "l" | "left" | "right" | "up" | "down"
                )))
}

fn append_boomux_setting_text(value: &mut String, text: &str) {
    for character in text.chars().filter(|c| !c.is_control() || *c == '\n') {
        if value.len() + character.len_utf8() > 16384 {
            break;
        }
        value.push(character);
    }
}

fn append_resource_name(value: &mut String, text: &str) {
    const MAX_RESOURCE_NAME_CHARS: usize = 128;
    let remaining = MAX_RESOURCE_NAME_CHARS.saturating_sub(value.chars().count());
    value.extend(
        text.chars()
            .filter(|character| !character.is_control())
            .take(remaining),
    );
}

fn relative_time(timestamp_ms: u64) -> String {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let seconds = now_ms.saturating_sub(timestamp_ms) / 1_000;
    match seconds {
        0..=59 => "now".into(),
        60..=3_599 => format!("{}m", seconds / 60),
        3_600..=86_399 => format!("{}h", seconds / 3_600),
        _ => format!("{}d", seconds / 86_400),
    }
}

type RenderedTerminalImage = (TerminalImagePlacement, Arc<RenderImage>);

fn same_text_run_style(left: &TextRun, right: &TextRun) -> bool {
    left.font == right.font
        && left.color == right.color
        && left.background_color == right.background_color
        && left.underline == right.underline
        && left.strikethrough == right.strikethrough
        && left.letter_spacing == right.letter_spacing
}

fn push_text_run(runs: &mut Vec<TextRun>, run: TextRun) {
    if let Some(previous) = runs.last_mut()
        && same_text_run_style(previous, &run)
    {
        previous.len += run.len;
    } else {
        runs.push(run);
    }
}

fn prepare_terminal_paint(
    screen: Arc<TerminalScreen>,
    selection: Option<TerminalSelection>,
    cursor_focused: bool,
    window: &mut Window,
) -> TerminalPaintCache {
    let terminal_theme = theme::current_terminal();
    let cols = usize::from(screen.cols);
    let mut lines = Vec::with_capacity(usize::from(screen.rows));
    let mut backgrounds = Vec::new();
    let mut cursor_outline = None;
    let mut base_font = font(if cfg!(target_os = "macos") {
        "Menlo"
    } else {
        "JetBrainsMono Nerd Font"
    });
    base_font.features = gpui::FontFeatures::disable_ligatures();
    let selection_range = selection.map(|selection| selection_indices(selection, cols));

    for (row, cells) in screen.cells.chunks(cols).enumerate() {
        let mut text = String::new();
        let mut runs = Vec::with_capacity(cells.len());
        for (col, cell) in cells.iter().enumerate() {
            let selected = selection_range.is_some_and(|(start, end)| {
                let index = row * cols + col;
                (start..=end).contains(&index)
            });
            let (foreground, background) = if selected {
                (
                    theme::resolve_legacy(0xcdd6f4),
                    theme::resolve_legacy(0xf5e0dc),
                )
            } else if cell.cursor && cursor_focused {
                (terminal_theme.background, terminal_theme.cursor)
            } else {
                (cell.foreground, cell.background)
            };
            if cell.cursor && !cursor_focused && !selected {
                cursor_outline = Some(TerminalBackground {
                    row,
                    col,
                    color: terminal_theme.cursor,
                });
            }
            if background != terminal_theme.background {
                backgrounds.push(TerminalBackground {
                    row,
                    col,
                    color: background,
                });
            }
            let start = text.len();
            // Keep one shaped glyph slot for every terminal cell. A wide
            // character paints from its leading cell; its continuation is an
            // inkless space so subsequent glyphs retain their exact columns.
            if cell.continuation {
                text.push(' ');
            } else {
                text.push_str(&cell.text);
            }
            let mut cell_font = base_font.clone();
            if cell.bold {
                cell_font = cell_font.bold();
            }
            if cell.italic {
                cell_font = cell_font.italic();
            }
            push_text_run(
                &mut runs,
                TextRun {
                    len: text.len() - start,
                    font: cell_font,
                    color: rgb_to_hsla(gpui::rgb(foreground)),
                    underline: cell.underline.then_some(UnderlineStyle {
                        thickness: px(1.0),
                        color: Some(rgb_to_hsla(gpui::rgb(foreground))),
                        wavy: false,
                    }),
                    ..Default::default()
                },
            );
        }
        lines.push(window.text_system().shape_line(
            text.into(),
            px(13.0),
            &runs,
            Some(px(TERMINAL_CELL_WIDTH)),
        ));
    }

    TerminalPaintCache {
        screen,
        selection,
        lines,
        backgrounds,
        cursor_focused,
        cursor_outline,
    }
}

fn terminal_view(paint_cache: Arc<TerminalPaintCache>, images: Vec<RenderedTerminalImage>) -> Div {
    let cached_paint = Arc::clone(&paint_cache);
    div().size_full().overflow_hidden().bg(rgb(0x11111b)).child(
        canvas(
            move |_, _, _| images,
            move |bounds, images, window, cx| {
                paint_terminal_images(bounds, &images, |z| z < i32::MIN / 2, window);
                for background in &cached_paint.backgrounds {
                    window.paint_quad(fill(
                        Bounds::new(
                            point(
                                bounds.left()
                                    + px(8.0 + background.col as f32 * TERMINAL_CELL_WIDTH),
                                bounds.top()
                                    + px(8.0 + background.row as f32 * TERMINAL_CELL_HEIGHT),
                            ),
                            size(px(TERMINAL_CELL_WIDTH), px(TERMINAL_CELL_HEIGHT)),
                        ),
                        gpui::rgb(background.color),
                    ));
                }
                paint_terminal_images(bounds, &images, |z| (i32::MIN / 2..0).contains(&z), window);
                for (row, line) in cached_paint.lines.iter().enumerate() {
                    let origin = point(
                        bounds.left() + px(8.0),
                        bounds.top() + px(8.0 + row as f32 * TERMINAL_CELL_HEIGHT),
                    );
                    let _ = line.paint(
                        origin,
                        px(TERMINAL_CELL_HEIGHT),
                        gpui::TextAlign::Left,
                        None,
                        window,
                        cx,
                    );
                }
                paint_terminal_images(bounds, &images, |z| z >= 0, window);
                if let Some(cursor) = cached_paint.cursor_outline {
                    window.paint_quad(gpui::outline(
                        Bounds::new(
                            point(
                                bounds.left() + px(8.0 + cursor.col as f32 * TERMINAL_CELL_WIDTH),
                                bounds.top() + px(8.0 + cursor.row as f32 * TERMINAL_CELL_HEIGHT),
                            ),
                            size(px(TERMINAL_CELL_WIDTH), px(TERMINAL_CELL_HEIGHT)),
                        ),
                        gpui::rgb(cursor.color),
                        gpui::BorderStyle::Solid,
                    ));
                }
            },
        )
        .size_full(),
    )
}

fn paint_terminal_images(
    terminal_bounds: Bounds<gpui::Pixels>,
    images: &[RenderedTerminalImage],
    layer: impl Fn(i32) -> bool,
    window: &mut Window,
) {
    for (placement, image) in images {
        if !layer(placement.z)
            || placement.source_width == 0
            || placement.source_height == 0
            || placement.cell_width == 0
            || placement.cell_height == 0
        {
            continue;
        }

        let cell_scale_x = TERMINAL_CELL_WIDTH / placement.cell_width as f32;
        let cell_scale_y = TERMINAL_CELL_HEIGHT / placement.cell_height as f32;
        let destination = Bounds::new(
            point(
                terminal_bounds.left()
                    + px(8.0
                        + placement.viewport_col as f32 * TERMINAL_CELL_WIDTH
                        + placement.x_offset as f32 * cell_scale_x),
                terminal_bounds.top()
                    + px(8.0
                        + placement.viewport_row as f32 * TERMINAL_CELL_HEIGHT
                        + placement.y_offset as f32 * cell_scale_y),
            ),
            size(
                px(placement.pixel_width as f32 * cell_scale_x),
                px(placement.pixel_height as f32 * cell_scale_y),
            ),
        );
        let image_scale_x = f32::from(destination.size.width) / placement.source_width as f32;
        let image_scale_y = f32::from(destination.size.height) / placement.source_height as f32;
        let image_size = image.size(0);
        let image_bounds = Bounds::new(
            point(
                destination.left() - px(placement.source_x as f32 * image_scale_x),
                destination.top() - px(placement.source_y as f32 * image_scale_y),
            ),
            size(
                px(image_size.width.0 as f32 * image_scale_x),
                px(image_size.height.0 as f32 * image_scale_y),
            ),
        );
        let clip = terminal_bounds.intersect(&destination);
        let _ = window.paint_image(
            clip,
            image_bounds,
            Corners::default(),
            Arc::clone(image),
            0,
            false,
        );
    }
}

fn main() {
    if subprocess::dispatch() {
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("boomux-desktop {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("--check-runtime") {
        if let Err(error) = runtime::check() {
            eprintln!("{error}");
            std::process::exit(1);
        }
        return;
    }
    let args = std::env::args_os().collect::<Vec<_>>();
    let update_ready = (args.get(1).is_some_and(|arg| arg == "--update-ready"))
        .then(|| args.get(2).map(std::path::PathBuf::from))
        .flatten();
    if args
        .get(1)
        .is_some_and(|arg| arg == boomux_settings::EDITOR_FLAG)
    {
        let result = if args.len() == 4 {
            boomux_settings::editor(
                std::path::Path::new(&args[2]),
                std::path::Path::new(&args[3]),
            )
        } else {
            Err("Expected request directory and editor working file".into())
        };
        if let Err(error) = result {
            eprintln!("{error}");
            std::process::exit(1);
        }
        return;
    }
    let loaded = settings::path()
        .ok_or_else(|| "Cannot resolve the Desktop settings directory".to_string())
        .and_then(|path| settings::Settings::load(&path));
    let (saved, settings_error) = match loaded {
        Ok(saved) => (saved, None),
        Err(error) => (settings::Settings::default(), Some(error)),
    };
    let application = gpui_platform::application();
    #[cfg(target_os = "macos")]
    application.on_reopen(|cx| {
        if cx.windows().is_empty() {
            let loaded = settings::path()
                .ok_or_else(|| "Cannot resolve Desktop settings".to_string())
                .and_then(|path| settings::Settings::load(&path));
            let (saved, error) = match loaded {
                Ok(saved) => (saved, None),
                Err(error) => (settings::Settings::default(), Some(error)),
            };
            open_desktop_window(cx, saved, error);
        }
        cx.activate(true);
    });
    application.run(move |cx: &mut App| {
        #[cfg(target_os = "macos")]
        cx.bind_keys([
            KeyBinding::new("cmd-c", CopySelection, Some("Terminal")),
            KeyBinding::new("cmd-v", PasteClipboard, Some("Terminal")),
            KeyBinding::new("cmd-v", PasteClipboard, Some("BoomuxSettingsInput")),
            KeyBinding::new("cmd-q", Quit, None),
        ]);
        #[cfg(target_os = "macos")]
        cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
        cx.bind_keys([
            KeyBinding::new("ctrl-shift-v", PasteClipboard, Some("BoomuxSettingsInput")),
            // Layout commands remain available while Control is held with the leader.
            KeyBinding::new("ctrl-h", FocusLeft, Some("Layout")),
            KeyBinding::new("ctrl-l", FocusRight, Some("Layout")),
            KeyBinding::new("ctrl-k", FocusUp, Some("Layout")),
            KeyBinding::new("ctrl-j", ToggleSplit, Some("Layout")),
            KeyBinding::new("ctrl-left", FocusLeft, Some("Layout")),
            KeyBinding::new("ctrl-right", FocusRight, Some("Layout")),
            KeyBinding::new("ctrl-up", FocusUp, Some("Layout")),
            KeyBinding::new("ctrl-down", FocusDown, Some("Layout")),
            KeyBinding::new("ctrl-shift-h", MoveLeft, Some("Layout")),
            KeyBinding::new("ctrl-shift-l", MoveRight, Some("Layout")),
            KeyBinding::new("ctrl-shift-k", MoveUp, Some("Layout")),
            KeyBinding::new("ctrl-shift-j", MoveDown, Some("Layout")),
            KeyBinding::new("ctrl-shift-left", MoveLeft, Some("Layout")),
            KeyBinding::new("ctrl-shift-right", MoveRight, Some("Layout")),
            KeyBinding::new("ctrl-shift-up", MoveUp, Some("Layout")),
            KeyBinding::new("ctrl-shift-down", MoveDown, Some("Layout")),
            KeyBinding::new("ctrl-alt-h", ResizeSmallLeft, Some("Layout")),
            KeyBinding::new("ctrl-alt-l", ResizeSmallRight, Some("Layout")),
            KeyBinding::new("ctrl-alt-k", ResizeSmallUp, Some("Layout")),
            KeyBinding::new("ctrl-alt-j", ResizeSmallDown, Some("Layout")),
            KeyBinding::new("ctrl-alt-left", ResizeLeft, Some("Layout")),
            KeyBinding::new("ctrl-alt-right", ResizeRight, Some("Layout")),
            KeyBinding::new("ctrl-alt-up", ResizeUp, Some("Layout")),
            KeyBinding::new("ctrl-alt-down", ResizeDown, Some("Layout")),
            KeyBinding::new("ctrl-alt-shift-h", ResizeLargeLeft, Some("Layout")),
            KeyBinding::new("ctrl-alt-shift-l", ResizeLargeRight, Some("Layout")),
            KeyBinding::new("ctrl-alt-shift-k", ResizeLargeUp, Some("Layout")),
            KeyBinding::new("ctrl-alt-shift-j", ResizeLargeDown, Some("Layout")),
            KeyBinding::new("ctrl-alt-shift-left", AlignFloatingLeft, Some("Layout")),
            KeyBinding::new("ctrl-alt-shift-right", AlignFloatingRight, Some("Layout")),
            KeyBinding::new("ctrl-alt-shift-up", AlignFloatingUp, Some("Layout")),
            KeyBinding::new("ctrl-alt-shift-down", AlignFloatingDown, Some("Layout")),
            KeyBinding::new("ctrl-s", ToggleSplit, Some("Layout")),
            KeyBinding::new("ctrl-e", EqualizeSplit, Some("Layout")),
            KeyBinding::new("ctrl-r", SwapSplit, Some("Layout")),
            KeyBinding::new("ctrl-c", CenterFloating, Some("Layout")),
            KeyBinding::new("ctrl-tab", CyclePaneNext, Some("Layout")),
            KeyBinding::new("ctrl-shift-tab", CyclePanePrevious, Some("Layout")),
            KeyBinding::new("ctrl-pagedown", CycleWorkspaceNext, Some("Layout")),
            KeyBinding::new("ctrl-pageup", CycleWorkspacePrevious, Some("Layout")),
            KeyBinding::new("ctrl-o", ToggleFloating, Some("Layout")),
            KeyBinding::new("ctrl-f", ToggleFullscreen, Some("Layout")),
            KeyBinding::new("ctrl-b", ToggleSidebarDrawer, Some("Layout")),
            KeyBinding::new("ctrl-escape", ExitLayoutMode, Some("Layout")),
            KeyBinding::new("ctrl-right", FocusRight, Some("SidebarLayout")),
            KeyBinding::new("ctrl-l", FocusRight, Some("SidebarLayout")),
            KeyBinding::new("ctrl-pagedown", CycleWorkspaceNext, Some("SidebarLayout")),
            KeyBinding::new("ctrl-pageup", CycleWorkspacePrevious, Some("SidebarLayout")),
            KeyBinding::new("ctrl-g", ToggleGitPanel, Some("Layout")),
            KeyBinding::new("ctrl-g", ToggleGitPanel, Some("SidebarLayout")),
            KeyBinding::new("shift-insert", PasteClipboard, Some("BoomuxSettingsInput")),
            KeyBinding::new("h", FocusLeft, Some("Layout")),
            KeyBinding::new("l", FocusRight, Some("Layout")),
            KeyBinding::new("k", FocusUp, Some("Layout")),
            KeyBinding::new("j", ToggleSplit, Some("Layout")),
            KeyBinding::new("left", FocusLeft, Some("Layout")),
            KeyBinding::new("right", FocusRight, Some("Layout")),
            KeyBinding::new("up", FocusUp, Some("Layout")),
            KeyBinding::new("down", FocusDown, Some("Layout")),
            KeyBinding::new("shift-h", MoveLeft, Some("Layout")),
            KeyBinding::new("shift-l", MoveRight, Some("Layout")),
            KeyBinding::new("shift-k", MoveUp, Some("Layout")),
            KeyBinding::new("shift-j", MoveDown, Some("Layout")),
            KeyBinding::new("shift-left", MoveLeft, Some("Layout")),
            KeyBinding::new("shift-right", MoveRight, Some("Layout")),
            KeyBinding::new("shift-up", MoveUp, Some("Layout")),
            KeyBinding::new("shift-down", MoveDown, Some("Layout")),
            KeyBinding::new("alt-h", ResizeSmallLeft, Some("Layout")),
            KeyBinding::new("alt-l", ResizeSmallRight, Some("Layout")),
            KeyBinding::new("alt-k", ResizeSmallUp, Some("Layout")),
            KeyBinding::new("alt-j", ResizeSmallDown, Some("Layout")),
            KeyBinding::new("alt-left", ResizeLeft, Some("Layout")),
            KeyBinding::new("alt-right", ResizeRight, Some("Layout")),
            KeyBinding::new("alt-up", ResizeUp, Some("Layout")),
            KeyBinding::new("alt-down", ResizeDown, Some("Layout")),
            KeyBinding::new("alt-shift-h", ResizeLargeLeft, Some("Layout")),
            KeyBinding::new("alt-shift-l", ResizeLargeRight, Some("Layout")),
            KeyBinding::new("alt-shift-k", ResizeLargeUp, Some("Layout")),
            KeyBinding::new("alt-shift-j", ResizeLargeDown, Some("Layout")),
            KeyBinding::new("alt-shift-left", AlignFloatingLeft, Some("Layout")),
            KeyBinding::new("alt-shift-right", AlignFloatingRight, Some("Layout")),
            KeyBinding::new("alt-shift-up", AlignFloatingUp, Some("Layout")),
            KeyBinding::new("alt-shift-down", AlignFloatingDown, Some("Layout")),
            KeyBinding::new("s", ToggleSplit, Some("Layout")),
            KeyBinding::new("e", EqualizeSplit, Some("Layout")),
            KeyBinding::new("r", SwapSplit, Some("Layout")),
            KeyBinding::new("c", CenterFloating, Some("Layout")),
            KeyBinding::new("tab", CyclePaneNext, Some("Layout")),
            KeyBinding::new("shift-tab", CyclePanePrevious, Some("Layout")),
            KeyBinding::new("pagedown", CycleWorkspaceNext, Some("Layout")),
            KeyBinding::new("pageup", CycleWorkspacePrevious, Some("Layout")),
            KeyBinding::new("o", ToggleFloating, Some("Layout")),
            KeyBinding::new("f", ToggleFullscreen, Some("Layout")),
            KeyBinding::new("b", ToggleSidebarDrawer, Some("Layout")),
            KeyBinding::new("escape", ExitLayoutMode, Some("Layout")),
            KeyBinding::new("right", FocusRight, Some("SidebarLayout")),
            KeyBinding::new("l", FocusRight, Some("SidebarLayout")),
            KeyBinding::new("pagedown", CycleWorkspaceNext, Some("SidebarLayout")),
            KeyBinding::new("pageup", CycleWorkspacePrevious, Some("SidebarLayout")),
            KeyBinding::new(KEY_NEW_PANE, NewPane, Some("Terminal")),
            KeyBinding::new(KEY_NEW_PANE, NewPane, Some("Layout")),
            KeyBinding::new(KEY_NEW_PANE, NewPane, Some("Sidebar")),
            KeyBinding::new(KEY_NEW_PANE, NewPane, Some("SidebarLayout")),
            KeyBinding::new(KEY_REMOVE_SHELL, RemoveShell, Some("Terminal")),
            KeyBinding::new(KEY_REMOVE_SHELL, RemoveShell, Some("Layout")),
            KeyBinding::new(KEY_REMOVE_SHELL, RemoveShell, Some("Sidebar")),
            KeyBinding::new(KEY_REMOVE_SHELL, RemoveShell, Some("SidebarLayout")),
            KeyBinding::new(KEY_DETACH_PANE, ClosePane, Some("Terminal")),
            KeyBinding::new(KEY_DETACH_PANE, ClosePane, Some("Layout")),
            KeyBinding::new(KEY_DETACH_PANE, ClosePane, Some("Sidebar")),
            KeyBinding::new(KEY_DETACH_PANE, ClosePane, Some("SidebarLayout")),
            KeyBinding::new("g", ToggleGitPanel, Some("Layout")),
            KeyBinding::new("g", ToggleGitPanel, Some("SidebarLayout")),
            KeyBinding::new(KEY_TOGGLE_HELP, ToggleHelp, Some("Terminal")),
            KeyBinding::new(KEY_TOGGLE_HELP, ToggleHelp, Some("Layout")),
            KeyBinding::new(KEY_TOGGLE_HELP, ToggleHelp, Some("Sidebar")),
            KeyBinding::new(KEY_TOGGLE_HELP, ToggleHelp, Some("SidebarLayout")),
            KeyBinding::new(KEY_TOGGLE_HELP, ToggleHelp, Some("Help")),
            KeyBinding::new(KEY_TOGGLE_SIDEBAR, ToggleSidebarFocus, Some("Terminal")),
            KeyBinding::new(KEY_TOGGLE_SIDEBAR, ToggleSidebarFocus, Some("Layout")),
            KeyBinding::new(KEY_TOGGLE_SIDEBAR, ToggleSidebarFocus, Some("Sidebar")),
            KeyBinding::new(
                KEY_TOGGLE_SIDEBAR,
                ToggleSidebarFocus,
                Some("SidebarLayout"),
            ),
            KeyBinding::new(KEY_RENAME_RESOURCE, RenameResource, Some("Terminal")),
            KeyBinding::new(KEY_RENAME_RESOURCE, RenameResource, Some("Layout")),
            KeyBinding::new(KEY_RENAME_RESOURCE, RenameResource, Some("Sidebar")),
            KeyBinding::new(KEY_RENAME_RESOURCE, RenameResource, Some("SidebarLayout")),
            KeyBinding::new("secondary-shift-c", CopySelection, Some("Terminal")),
            KeyBinding::new("secondary-shift-c", CopySelection, Some("Layout")),
            KeyBinding::new("secondary-shift-c", CopySelection, Some("Sidebar")),
            KeyBinding::new("secondary-shift-c", CopySelection, Some("SidebarLayout")),
            KeyBinding::new("secondary-shift-v", PasteClipboard, Some("Terminal")),
            KeyBinding::new("secondary-shift-v", PasteClipboard, Some("Layout")),
            KeyBinding::new("secondary-shift-v", PasteClipboard, Some("Sidebar")),
            KeyBinding::new("secondary-shift-v", PasteClipboard, Some("SidebarLayout")),
            // Omarchy's universal Super+C / Super+V bindings translate terminal
            // clipboard actions to these conventional terminal chords.
            KeyBinding::new("ctrl-insert", CopySelection, Some("Terminal")),
            KeyBinding::new("ctrl-insert", CopySelection, Some("Layout")),
            KeyBinding::new("ctrl-insert", CopySelection, Some("Sidebar")),
            KeyBinding::new("ctrl-insert", CopySelection, Some("SidebarLayout")),
            KeyBinding::new("shift-insert", PasteClipboard, Some("Terminal")),
            KeyBinding::new("shift-insert", PasteClipboard, Some("Layout")),
            KeyBinding::new("shift-insert", PasteClipboard, Some("Sidebar")),
            KeyBinding::new("shift-insert", PasteClipboard, Some("SidebarLayout")),
        ]);

        open_desktop_window(cx, saved, settings_error);
        cx.activate(true);
        if let Some(path) = update_ready {
            bundle_update::signal_ready(path);
        }
    });
}

#[cfg(target_os = "macos")]
gpui::actions!(macos, [Quit]);

fn open_desktop_window(cx: &mut App, saved: settings::Settings, settings_error: Option<String>) {
    let mut layout_session = layout_state::Session::load();
    match boomux::client::connect_if_running()
        .ok()
        .flatten()
        .and_then(|client| client.node_identity().ok())
    {
        Some(owner)
            if layout_session.document.owner.is_empty()
                || layout_session.document.owner == owner =>
        {
            layout_session.document.owner = owner
        }
        _ => {
            layout_session.error = Some(
                "Saved layout belongs to an unavailable or different Node; it was retained.".into(),
            );
            layout_session.writer = None;
            layout_session.document = layout_state::Document::default();
        }
    }
    let bounds = Bounds::centered(None, gpui::size(px(1180.0), px(760.0)), cx);
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            // Omarchy tags org.omarchy.* windows as terminals, which makes
            // its universal clipboard binding choose Ctrl/Shift+Insert.
            app_id: Some("org.omarchy.boomux-desktop".into()),
            ..Default::default()
        },
        move |window, cx| {
            cx.new(|cx| Workspace::new(window, cx, saved, settings_error, layout_session))
        },
    )
    .unwrap();
}

#[cfg(test)]
mod pointer_tests {
    use super::*;

    #[test]
    fn layout_leader_release_distinguishes_taps_holds_and_toggle_off() {
        assert!(!layout_leader_release_exits(
            true,
            Duration::from_millis(249)
        ));
        assert!(layout_leader_release_exits(
            true,
            Duration::from_millis(250)
        ));
        assert!(layout_leader_release_exits(true, Duration::from_secs(2)));
        // A press that toggled off, or a chord already released via Control,
        // must not perform another mode transition when Space is released.
        assert!(!layout_leader_release_exits(false, Duration::from_secs(2)));
        for key in ["ctrl-left", "ctrl-tab", "ctrl-alt-shift-left"] {
            let _ = KeyBinding::new(key, FocusLeft, Some("Layout"));
        }
    }

    #[test]
    fn layout_leader_repeat_never_starts_another_gesture() {
        assert!(layout_leader_starts_press(false, false));
        assert!(!layout_leader_starts_press(false, true));
        for _ in 0..100 {
            assert!(!layout_leader_starts_press(true, true));
            // Focus/activation or release processing may clear tracked state.
            // A repeat is still not a fresh press and must never toggle Layout.
            assert!(!layout_leader_starts_press(true, false));
        }
        assert!(layout_leader_release_exits(true, Duration::from_secs(2)));
        assert!(layout_leader_starts_press(false, false));
    }

    #[test]
    fn sidebar_drag_collapses_at_edge_and_reopens_with_bounded_width() {
        for x in [-20.0, 0.0, 48.0] {
            assert_eq!(sidebar_drag_target(x), None);
        }
        assert_eq!(sidebar_drag_target(49.0), Some(280.0));
        assert_eq!(sidebar_drag_target(420.0), Some(420.0));
        assert_eq!(sidebar_drag_target(900.0), Some(600.0));
        // A single drag can cross the collapse boundary in either direction.
        assert_eq!(
            [320.0, 20.0, 100.0].map(sidebar_drag_target),
            [Some(320.0), None, Some(280.0)]
        );
    }

    #[test]
    fn settings_scroll_eliminates_hidden_sidebar_row_preparation() {
        let fixture = sidebar_overview();
        let mut overview = BoomuxOverview {
            workspaces: (0..100)
                .map(|index| {
                    let mut workspace = fixture.workspaces[0].clone();
                    workspace.id = format!("workspace-{index}");
                    workspace.shells = (0..20)
                        .map(|shell_index| {
                            let mut shell = workspace.shells[0].clone();
                            shell.id = format!("shell-{index}-{shell_index}");
                            shell
                        })
                        .collect();
                    workspace
                })
                .collect(),
            agents: vec![fixture.agents[0].clone(); 2000],
            ..BoomuxOverview::default()
        };
        // Same fixture/profile: isolate the formerly unconditional clone/row preparation,
        // not GPU painting or a claim about end-to-end scroll frame rate.
        let prepare_rows = |workspaces: &[terminal::WorkspaceChoice], agents: &[AgentChoice]| {
            let mut rows = 0;
            for workspace in workspaces.iter().cloned() {
                let shells = workspace.shells.clone();
                rows += 1 + shells.len();
                std::hint::black_box(shells);
            }
            let agents = agents.to_vec();
            rows += agents.len();
            std::hint::black_box(agents);
            rows
        };
        let before = Instant::now();
        let before_rows: usize = (0..60)
            .map(|_| prepare_rows(&overview.workspaces, &overview.agents))
            .sum();
        let before_time = before.elapsed();
        let after = Instant::now();
        let after_rows: usize = (0..60)
            .map(|_| {
                let (workspaces, agents) = sidebar_render_sources(&overview, true);
                prepare_rows(workspaces, agents)
            })
            .sum();
        let after_time = after.elapsed();
        assert_eq!(before_rows, 246000);
        assert_eq!(after_rows, 0);
        eprintln!(
            "60 Settings scroll preparations: before={before_rows} rows/{before_time:?}, after={after_rows} rows/{after_time:?}"
        );
        let (workspaces, agents) = sidebar_render_sources(&overview, false);
        assert_eq!(workspaces.len(), 100);
        assert_eq!(agents.len(), 2000);
        overview.workspaces.clear();
        overview.agents.clear();
        let (workspaces, agents) = sidebar_render_sources(&overview, false);
        assert!(
            workspaces.is_empty() && agents.is_empty(),
            "closing Settings must expose current data, not a stale cache"
        );
    }

    #[test]
    fn settings_controls_construct_enabled_and_disabled_selected_states() {
        for selected in [false, true] {
            let mut enabled = Workspace::settings_control("enabled", "On", selected, true);
            let mut disabled = Workspace::settings_control("disabled", "On", selected, false);
            let mut option = Workspace::settings_option("option", "On", selected);
            assert_eq!(enabled.style().background, disabled.style().background);
            assert_eq!(enabled.style().border_color, disabled.style().border_color);
            assert_eq!(enabled.style().background, option.style().background);
            assert_eq!(
                enabled.style().mouse_cursor,
                Some(gpui::CursorStyle::PointingHand)
            );
            assert_eq!(
                disabled.style().mouse_cursor,
                Some(gpui::CursorStyle::Arrow)
            );
            // GPUI rejects a second hover style; disabled controls must leave it unset.
            let _ = disabled.hover(|style| style);
        }
    }

    #[test]
    fn sidebar_header_controls_share_geometry_and_distinguish_active_state() {
        let mut create = sidebar_header_button("create-workspace", "New Workspace", "+", false);
        let mut settings = sidebar_header_button("open-settings", "Settings", "⚙", false);
        let mut menu = sidebar_header_button("open-sidebar-menu", "More actions", "⋯", false);
        for button in [&mut create, &mut settings, &mut menu] {
            assert_eq!(button.style().size.width, Some(px(28.0).into()));
            assert_eq!(button.style().size.height, Some(px(28.0).into()));
            assert_eq!(button.style().flex_shrink, Some(0.0));
        }
        assert_eq!(create.style().border_color, settings.style().border_color);
        assert_eq!(settings.style().border_color, menu.style().border_color);
        let mut active = sidebar_header_button("open-settings", "Settings", "⚙", true);
        assert_ne!(settings.style().border_color, active.style().border_color);
    }

    #[test]
    fn sidebar_overflow_rows_are_flat_and_left_aligned() {
        for id in [
            "header-menu-nodes",
            "header-menu-setup",
            "header-menu-updates",
            "header-menu-help",
            "header-menu-hide-sidebar",
        ] {
            let mut row = sidebar_menu_row(id);
            assert_eq!(row.style().size.height, Some(px(36.0).into()));
            assert_eq!(row.style().justify_content, None);
            assert_eq!(row.style().border_color, None);
            assert_eq!(row.style().background, None);
        }
    }
    use boomux::protocol::{AgentState, ShellStatus};

    #[test]
    fn lifted_pane_follows_pointer_beyond_panel_edges_without_resizing() {
        let start = FloatingPane {
            id: 7,
            x: 0.0,
            y: 0.0,
            width: 1000.0,
            height: 800.0,
        };
        for delta in [(-200.0, -150.0), (300.0, 250.0), (0.0, 0.0)] {
            let moved = lifted_drag_bounds(start.clone(), delta);
            assert_eq!((moved.x, moved.y), delta);
            assert_eq!((moved.width, moved.height), (1000.0, 800.0));
            assert_eq!(moved.id, start.id);
        }
    }

    #[test]
    fn corner_drag_resizes_both_axes_and_preserves_opposite_corner() {
        for (horizontal, vertical, expected) in [
            (Direction::Left, Direction::Up, (130.0, 140.0, 370.0, 260.0)),
            (
                Direction::Right,
                Direction::Up,
                (100.0, 140.0, 430.0, 260.0),
            ),
            (
                Direction::Left,
                Direction::Down,
                (130.0, 100.0, 370.0, 340.0),
            ),
            (
                Direction::Right,
                Direction::Down,
                (100.0, 100.0, 430.0, 340.0),
            ),
        ] {
            let start = FloatingPane {
                id: 1,
                x: 100.0,
                y: 100.0,
                width: 400.0,
                height: 300.0,
            };
            let result = dragged_bounds(
                start,
                PointerOperation::ResizeCorner(horizontal, vertical),
                (30.0, 40.0),
                (1000.0, 800.0),
            );
            assert_eq!((result.x, result.y, result.width, result.height), expected);
        }
    }

    #[test]
    fn edge_drag_floating_keeps_opposite_edge_fixed_and_bounds_size() {
        let start = FloatingPane {
            id: 1,
            x: 100.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
        };
        let left = dragged_bounds(
            start.clone(),
            PointerOperation::ResizeEdge(Direction::Left),
            (50.0, 20.0),
            (1000.0, 800.0),
        );
        assert_eq!(
            (left.x, left.width, left.y, left.height),
            (150.0, 350.0, 100.0, 300.0)
        );
        let top = dragged_bounds(
            start.clone(),
            PointerOperation::ResizeEdge(Direction::Up),
            (20.0, 50.0),
            (1000.0, 800.0),
        );
        assert_eq!(
            (top.y, top.height, top.x, top.width),
            (150.0, 250.0, 100.0, 400.0)
        );
        let minimum = dragged_bounds(
            start.clone(),
            PointerOperation::ResizeEdge(Direction::Left),
            (1000.0, 0.0),
            (1000.0, 800.0),
        );
        assert_eq!(minimum.width, MIN_FLOAT_WIDTH);
        assert_eq!(minimum.x + minimum.width, 500.0);
        let right = dragged_bounds(
            start.clone(),
            PointerOperation::ResizeEdge(Direction::Right),
            (1000.0, 0.0),
            (1000.0, 800.0),
        );
        assert_eq!((right.x, right.width), (100.0, 900.0));
        let bottom = dragged_bounds(
            start,
            PointerOperation::ResizeEdge(Direction::Down),
            (0.0, 1000.0),
            (1000.0, 800.0),
        );
        assert_eq!((bottom.y, bottom.height), (100.0, 700.0));
    }

    fn pane() -> FloatingPane {
        FloatingPane {
            id: 1,
            x: 100.0,
            y: 80.0,
            width: 400.0,
            height: 240.0,
        }
    }

    #[test]
    fn adjacent_terminal_text_runs_with_the_same_style_are_coalesced() {
        let mut runs = Vec::new();
        push_text_run(
            &mut runs,
            TextRun {
                len: 1,
                color: rgb_to_hsla(rgb(0xcdd6f4)),
                ..Default::default()
            },
        );
        push_text_run(
            &mut runs,
            TextRun {
                len: 3,
                color: rgb_to_hsla(rgb(0xcdd6f4)),
                ..Default::default()
            },
        );
        push_text_run(
            &mut runs,
            TextRun {
                len: 2,
                color: rgb_to_hsla(rgb(0xf38ba8)),
                ..Default::default()
            },
        );

        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].len, 4);
        assert_eq!(runs[1].len, 2);
    }

    #[test]
    fn pointer_coordinates_use_the_terminal_panel_origin() {
        assert_eq!(
            window_point_to_panel(425.0, 92.0, SIDEBAR_WIDTH),
            (125.0, 92.0)
        );
        assert_eq!(window_point_to_panel(425.0, 92.0, 0.0), (425.0, 92.0));
    }

    #[test]
    fn keyboard_sidebar_focus_ignores_stationary_pointer_hover() {
        let anchor = (900.0, 400.0);
        assert!(!pointer_moved_from(anchor, anchor));
        assert!(!pointer_moved_from(anchor, (900.4, 400.4)));
        assert!(pointer_moved_from(anchor, (901.0, 400.0)));
        assert!(pointer_moved_from(anchor, (900.0, 399.0)));
    }

    #[test]
    fn shift_enter_and_ctrl_c_remain_terminal_input() {
        assert!(!desktop_keystroke(
            &gpui::Keystroke {
                key: "enter".into(),
                key_char: None,
                modifiers: gpui::Modifiers {
                    shift: true,
                    ..Default::default()
                },
            },
            false,
        ));
        assert!(!desktop_keystroke(
            &gpui::Keystroke {
                key: "c".into(),
                key_char: Some("c".into()),
                modifiers: gpui::Modifiers {
                    control: true,
                    ..Default::default()
                },
            },
            false,
        ));
    }

    #[test]
    fn layout_commands_are_reserved_only_in_layout_mode() {
        let left = gpui::Keystroke {
            key: "left".into(),
            key_char: None,
            modifiers: gpui::Modifiers::default(),
        };
        let ctrl_left = gpui::Keystroke {
            modifiers: gpui::Modifiers {
                control: true,
                ..Default::default()
            },
            ..left.clone()
        };
        let leader = gpui::Keystroke {
            key: "space".into(),
            key_char: None,
            modifiers: gpui::Modifiers {
                control: true,
                ..Default::default()
            },
        };
        let copy = gpui::Keystroke {
            key: "c".into(),
            key_char: Some("c".into()),
            modifiers: gpui::Modifiers {
                control: !cfg!(target_os = "macos"),
                platform: cfg!(target_os = "macos"),
                shift: true,
                ..Default::default()
            },
        };

        assert!(!desktop_keystroke(&left, false));
        assert!(desktop_keystroke(&left, true));
        assert!(!desktop_keystroke(&ctrl_left, false));
        assert!(!desktop_keystroke(&ctrl_left, true));
        assert!(desktop_keystroke(&leader, false));
        assert!(desktop_keystroke(&leader, true));
        assert!(desktop_keystroke(&copy, false));
        assert!(desktop_keystroke(&copy, true));
    }

    #[test]
    fn double_layout_leader_passthrough_has_a_bounded_window() {
        assert!(layout_leader_passes_through(Duration::ZERO));
        assert!(layout_leader_passes_through(
            LAYOUT_LEADER_PASSTHROUGH_WINDOW
        ));
        assert!(!layout_leader_passes_through(
            LAYOUT_LEADER_PASSTHROUGH_WINDOW + Duration::from_millis(1)
        ));
    }

    #[test]
    fn sidebar_focus_preserves_layout_mode_with_boundary_navigation() {
        assert_eq!(
            workspace_key_context(false, NavigationRegion::Terminal, true),
            "Layout"
        );
        assert_eq!(
            workspace_key_context(false, NavigationRegion::Sidebar, true),
            "SidebarLayout"
        );
        assert_eq!(
            workspace_key_context(false, NavigationRegion::Terminal, false),
            "Terminal"
        );
    }

    #[test]
    fn layout_badge_text_contrasts_with_light_and_dark_accents() {
        assert_eq!(contrast_foreground(0xf0f0f0), 0x111111);
        assert_eq!(contrast_foreground(0x202040), 0xffffff);
    }

    #[test]
    fn layout_key_binding_spellings_are_valid_gpui_inputs() {
        for input in [
            "h",
            "left",
            "shift-h",
            "shift-left",
            "alt-h",
            "alt-left",
            "alt-shift-h",
            "alt-shift-left",
            "tab",
            "shift-tab",
            "pageup",
            "pagedown",
            "ctrl-space",
        ] {
            let _ = KeyBinding::new(input, FocusLeft, Some("Layout"));
        }
    }

    #[test]
    fn ordinary_ctrl_letters_are_not_confused_with_modified_desktop_actions() {
        for key in ["b", "f", "h", "j", "k", "l", "o", "s", "e", "r", "c"] {
            assert!(!desktop_keystroke(
                &gpui::Keystroke {
                    key: key.into(),
                    key_char: Some(key.into()),
                    modifiers: gpui::Modifiers {
                        control: true,
                        ..Default::default()
                    },
                },
                false,
            ));
        }

        assert!(desktop_keystroke(
            &gpui::Keystroke {
                key: "enter".into(),
                key_char: None,
                modifiers: gpui::Modifiers {
                    control: true,
                    ..Default::default()
                },
            },
            false,
        ));
    }

    #[test]
    fn zero_spacing_removes_the_outer_workspace_inset() {
        assert_eq!(inset_panel_size((1200.0, 800.0), 0.0), (1200.0, 800.0));
        assert_eq!(inset_panel_size((1200.0, 800.0), 8.0), (1184.0, 784.0));
    }

    #[test]
    fn window_title_includes_the_focused_workspace() {
        assert_eq!(
            desktop_window_title(Some("lively-dolphin")),
            "Boomux Desktop — lively-dolphin"
        );
        assert_eq!(desktop_window_title(None), "Boomux Desktop");
    }

    #[test]
    fn layout_rects_interpolate_without_overshoot() {
        let from = Rect {
            x: 0.5,
            y: 0.25,
            width: 0.5,
            height: 0.75,
        };
        let to = Rect {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
        };
        assert_eq!(interpolate_rect(from, to, 0.0).x, 0.5);
        let middle = interpolate_rect(from, to, 0.5);
        assert_eq!(middle.x, 0.25);
        assert_eq!(middle.width, 0.75);
        assert_eq!(interpolate_rect(from, to, 1.0).width, 1.0);
    }

    #[test]
    fn workspace_maximize_preserves_layout_and_paints_the_target_last() {
        let mut layout = Node::pane(1);
        layout.split(1, 2, Axis::Horizontal);
        let original = layout.rects();
        let maximized = workspace_layout_rects(&layout, Some(1));

        assert_eq!(maximized.last().unwrap().0, 1);
        let target = maximized.last().unwrap().1;
        assert_eq!(target.x, 0.0);
        assert_eq!(target.y, 0.0);
        assert_eq!(target.width, 1.0);
        assert_eq!(target.height, 1.0);

        let restored = workspace_layout_rects(&layout, None);
        for ((original_id, original), (restored_id, restored)) in
            original.iter().copied().zip(restored.iter().copied())
        {
            assert_eq!(original_id, restored_id);
            assert_eq!(original.x, restored.x);
            assert_eq!(original.y, restored.y);
            assert_eq!(original.width, restored.width);
            assert_eq!(original.height, restored.height);
        }

        let mut restoring = restored;
        paint_layout_pane_last(&mut restoring, Some(1));
        assert_eq!(restoring.last().unwrap().0, 1);
        assert_eq!(
            restoring.last().unwrap().1.width,
            original.iter().find(|(id, _)| *id == 1).unwrap().1.width
        );
    }

    #[test]
    fn floating_workspace_maximize_respects_canvas_spacing() {
        let maximized = workspace_maximized_pane(7, (1_200.0, 800.0), 8.0);
        assert_eq!(maximized.id, 7);
        assert_eq!(maximized.x, 8.0);
        assert_eq!(maximized.y, 8.0);
        assert_eq!(maximized.width, 1_176.0);
        assert_eq!(maximized.height, 776.0);
    }

    #[test]
    fn keyboard_swap_animates_each_pane_from_its_previous_rect() {
        let mut layout = Node::pane(1);
        layout.split(1, 2, Axis::Horizontal);
        let before = layout.rects().into_iter().collect::<HashMap<_, _>>();

        let animation = swap_layout_direction(&mut layout, 1, Direction::Right).unwrap();
        let after = layout.rects().into_iter().collect::<HashMap<_, _>>();

        assert_eq!(animation.get(&1).unwrap().x, before.get(&1).unwrap().x);
        assert_eq!(animation.get(&2).unwrap().x, before.get(&2).unwrap().x);
        assert_eq!(after.get(&1).unwrap().x, before.get(&2).unwrap().x);
        assert_eq!(after.get(&2).unwrap().x, before.get(&1).unwrap().x);

        let mut paint_order = layout.rects();
        paint_layout_pane_last(&mut paint_order, Some(1));
        assert_eq!(paint_order.last().unwrap().0, 1);
    }

    #[test]
    fn workspace_switch_slides_in_the_sidebar_order_direction() {
        let order = vec![
            "alpha".to_string(),
            "bravo".to_string(),
            "charlie".to_string(),
        ];
        assert_eq!(
            workspace_slide_direction(&order, Some("alpha"), "charlie"),
            1.0
        );
        assert_eq!(
            workspace_slide_direction(&order, Some("charlie"), "alpha"),
            -1.0
        );

        let target = Rect {
            x: 0.25,
            y: 0.0,
            width: 0.5,
            height: 1.0,
        };
        assert_eq!(shifted_workspace_rect(target, 1.0).x, 1.25);
        assert_eq!(shifted_workspace_rect(target, -1.0).x, -0.75);
    }

    #[test]
    fn workspace_cycle_wraps_in_both_directions() {
        let order = vec![
            "alpha".to_string(),
            "bravo".to_string(),
            "charlie".to_string(),
        ];

        assert_eq!(
            cycled_workspace_id(&order, Some("alpha"), false),
            Some("bravo")
        );
        assert_eq!(
            cycled_workspace_id(&order, Some("charlie"), false),
            Some("alpha")
        );
        assert_eq!(
            cycled_workspace_id(&order, Some("alpha"), true),
            Some("charlie")
        );
        assert_eq!(cycled_workspace_id(&order, None, false), Some("alpha"));
        assert_eq!(cycled_workspace_id(&order, None, true), Some("charlie"));
        assert_eq!(cycled_workspace_id(&[], None, false), None);
    }

    #[test]
    fn terminal_selection_drag_uses_only_source_pane_bounds_and_mouse_down_anchor() {
        let screen = TerminalScreen {
            rows: 24,
            cols: 80,
            cells: Vec::new(),
            scroll_total: 24,
            scroll_offset: 0,
            scroll_len: 24,
            images: Vec::new(),
            image_placements: Vec::new(),
        };
        let bounds = Bounds::new(point(px(600.0), px(100.0)), size(px(700.0), px(430.0)));
        let position = |row: usize, col: usize| {
            bounds.origin
                + point(
                    px(8.0 + (col as f32 + 0.5) * TERMINAL_CELL_WIDTH),
                    px(8.0 + (row as f32 + 0.5) * TERMINAL_CELL_HEIGHT),
                )
        };
        let drag = TerminalSelectionDrag { pane_id: 2 };
        let anchor = Some(position(3, 20));
        for head in [(3, 30), (3, 10), (2, 40), (5, 10)] {
            assert_eq!(
                drag.selection(2, anchor, position(head.0, head.1), bounds, &screen),
                Some(TerminalSelection {
                    anchor: (3, 20),
                    head
                }),
            );
            // A neighboring pane receives the same move with different bounds.
            // It must neither move the anchor nor consume the source's update.
            let neighbor = Bounds::new(point(px(0.0), px(0.0)), bounds.size);
            assert_eq!(
                drag.selection(1, anchor, position(head.0, head.1), neighbor, &screen),
                None,
            );
        }
        assert_eq!(
            drag.selection(2, anchor, point(px(0.0), px(0.0)), bounds, &screen),
            Some(TerminalSelection {
                anchor: (3, 20),
                head: (0, 0)
            }),
        );
        assert_eq!(
            drag.selection(2, anchor, point(px(2000.0), px(2000.0)), bounds, &screen),
            Some(TerminalSelection {
                anchor: (3, 20),
                head: (23, 79)
            }),
        );
        assert_eq!(
            drag.selection(2, None, position(3, 30), bounds, &screen),
            None
        );
        // A new gesture uses its own mouse-down position, even before the first
        // delivered drag move and regardless of the previous selection.
        assert_eq!(
            drag.selection(2, Some(position(7, 50)), position(7, 45), bounds, &screen),
            Some(TerminalSelection {
                anchor: (7, 50),
                head: (7, 45)
            }),
        );
    }

    fn selection_test_screen() -> TerminalScreen {
        let cells = "abc efg "
            .chars()
            .map(|character| terminal::TerminalCell {
                text: character.to_string().into(),
                foreground: 0xffffff,
                background: 0,
                bold: false,
                italic: false,
                underline: false,
                wide: false,
                continuation: false,
                cursor: false,
            })
            .collect();
        TerminalScreen {
            rows: 2,
            cols: 4,
            cells,
            scroll_total: 2,
            scroll_offset: 0,
            scroll_len: 2,
            images: Vec::new(),
            image_placements: Vec::new(),
        }
    }

    #[test]
    fn terminal_selection_extracts_rows_in_either_drag_direction() {
        let screen = selection_test_screen();
        let selection = TerminalSelection {
            anchor: (1, 1),
            head: (0, 1),
        };
        assert_eq!(terminal_selected_text(&screen, selection), "bc\nef");
    }

    #[test]
    fn terminal_selection_copy_on_release_is_optional_nonempty_and_once_per_gesture() {
        let mut panes = HashMap::from([(
            2,
            TerminalPane {
                screen: Some(Arc::new(selection_test_screen())),
                selection: Some(TerminalSelection {
                    anchor: (0, 1),
                    head: (1, 1),
                }),
                ..TerminalPane::default()
            },
        )]);
        let mut pending = Some(2);
        assert_eq!(
            take_terminal_selection_copy(&mut pending, &panes, true).as_deref(),
            Some("bc\nef")
        );
        assert_eq!(pending, None);
        assert_eq!(
            take_terminal_selection_copy(&mut pending, &panes, true),
            None
        );
        pending = Some(2);
        assert_eq!(
            take_terminal_selection_copy(&mut pending, &panes, false),
            None
        );
        assert_eq!(pending, None);
        // Disabling automatic copy preserves the selection for manual copying.
        assert!(panes[&2].selection.is_some());
        pending = Some(99);
        assert_eq!(
            take_terminal_selection_copy(&mut pending, &panes, true),
            None
        );
        assert_eq!(pending, None);
        panes.get_mut(&2).unwrap().selection = Some(TerminalSelection {
            anchor: (0, 3),
            head: (0, 3),
        });
        pending = Some(2);
        assert_eq!(
            take_terminal_selection_copy(&mut pending, &panes, true),
            None
        );
        panes.get_mut(&2).unwrap().selection = None;
        pending = Some(2);
        assert_eq!(
            take_terminal_selection_copy(&mut pending, &panes, true),
            None
        );
    }

    #[test]
    fn moving_is_clamped_to_panel() {
        let moved = dragged_bounds(
            pane(),
            PointerOperation::Move,
            (900.0, -900.0),
            (800.0, 600.0),
        );
        assert_eq!(moved.x, 400.0);
        assert_eq!(moved.y, 0.0);
    }

    #[test]
    fn floating_pane_is_clamped_when_the_drawer_reduces_the_panel() {
        let clamped = clamp_floating_to_panel(
            FloatingPane {
                x: 700.0,
                width: 400.0,
                ..pane()
            },
            (800.0, 600.0),
        );
        assert_eq!(clamped.x, 400.0);
        assert_eq!(clamped.width, 400.0);
    }

    #[test]
    fn newly_floating_pane_is_large_and_centered_within_the_panel() {
        let pane = centered_floating_pane(7, (1_600.0, 900.0), 8.0);
        assert_eq!(pane.id, 7);
        assert_eq!(pane.width, 1_100.0);
        assert_eq!(pane.height, 648.0);
        assert_eq!(pane.x, 250.0);
        assert_eq!(pane.y, 126.0);
    }

    #[test]
    fn floating_transition_interpolates_position_and_size() {
        let from = FloatingPane {
            id: 4,
            x: 0.0,
            y: 20.0,
            width: 400.0,
            height: 300.0,
        };
        let to = FloatingPane {
            id: 4,
            x: 200.0,
            y: 100.0,
            width: 800.0,
            height: 600.0,
        };
        let midpoint = interpolate_floating_pane(&from, &to, 0.5);
        assert_eq!(midpoint.x, 100.0);
        assert_eq!(midpoint.y, 60.0);
        assert_eq!(midpoint.width, 600.0);
        assert_eq!(midpoint.height, 450.0);
    }

    #[test]
    fn floating_alignment_respects_canvas_edges_and_center() {
        let pane = FloatingPane {
            id: 4,
            x: 200.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
        };
        let left =
            align_floating_to_panel(pane.clone(), FloatingAlignment::Left, (1_000.0, 800.0), 8.0);
        let right = align_floating_to_panel(
            pane.clone(),
            FloatingAlignment::Right,
            (1_000.0, 800.0),
            8.0,
        );
        let centered =
            align_floating_to_panel(pane, FloatingAlignment::Center, (1_000.0, 800.0), 8.0);

        assert_eq!(left.x, 8.0);
        assert_eq!(right.x, 592.0);
        assert_eq!((centered.x, centered.y), (300.0, 250.0));
    }

    #[test]
    fn floating_keyboard_resize_supports_grow_and_shrink() {
        let pane = FloatingPane {
            id: 4,
            x: 200.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
        };
        let left =
            resize_floating_in_direction(pane.clone(), Direction::Left, 32.0, (1_000.0, 800.0));
        let up = resize_floating_in_direction(pane.clone(), Direction::Up, 32.0, (1_000.0, 800.0));
        let right =
            resize_floating_in_direction(pane.clone(), Direction::Right, 32.0, (1_000.0, 800.0));
        let down = resize_floating_in_direction(pane, Direction::Down, 32.0, (1_000.0, 800.0));

        assert_eq!((left.x, left.width), (200.0, 368.0));
        assert_eq!((up.y, up.height), (100.0, 268.0));
        assert_eq!(right.width, 432.0);
        assert_eq!(down.height, 332.0);
    }

    #[test]
    fn pane_cycle_wraps_in_both_directions() {
        let panes = [3, 7, 11];
        assert_eq!(cycled_pane_id(&panes, 3, false), Some(7));
        assert_eq!(cycled_pane_id(&panes, 11, false), Some(3));
        assert_eq!(cycled_pane_id(&panes, 3, true), Some(11));
        assert_eq!(cycled_pane_id(&[], 3, false), None);
    }

    #[test]
    fn minimized_tabs_are_default_and_hide_shell_rows_from_the_sidebar() {
        assert_eq!(PaneLayoutMode::default(), PaneLayoutMode::Tabbed);
        assert!(pane_layout_supports_scope(
            PaneLayoutMode::Tiled,
            WorkspacePaneMode::Mixed
        ));
        assert!(!pane_layout_supports_scope(
            PaneLayoutMode::Tabbed,
            WorkspacePaneMode::Mixed
        ));

        let mut layout = Node::pane(1);
        layout.split(1, 3, Axis::Horizontal);
        let floating = vec![
            FloatingPane {
                id: 9,
                x: 0.0,
                y: 0.0,
                width: 320.0,
                height: 240.0,
            },
            FloatingPane {
                id: 5,
                x: 20.0,
                y: 20.0,
                width: 320.0,
                height: 240.0,
            },
        ];

        let panes = ordered_pane_ids(Some(&layout), &floating);
        assert_eq!(panes, vec![1, 3, 5, 9]);

        let workspace = SidebarItem::Workspace("workspace-1".into());
        let shell = SidebarItem::Shell {
            workspace_id: "workspace-1".into(),
            shell_id: "shell-1".into(),
        };
        let other_shell = SidebarItem::Shell {
            workspace_id: "workspace-1".into(),
            shell_id: "shell-2".into(),
        };
        assert!(sidebar_item_visible_in_layout(
            PaneLayoutMode::Tabbed,
            &workspace
        ));
        assert!(!sidebar_item_visible_in_layout(
            PaneLayoutMode::Tabbed,
            &shell
        ));
        assert!(!sidebar_item_visible_in_layout(
            PaneLayoutMode::Tabbed,
            &other_shell
        ));
        assert!(sidebar_item_visible_in_layout(
            PaneLayoutMode::Tiled,
            &shell
        ));
    }

    #[test]
    fn motion_speed_offers_instant_fast_and_smooth_timings() {
        assert_eq!(MotionSpeed::default(), MotionSpeed::Smooth);
        assert_eq!(MotionSpeed::Instant.duration(), None);
        assert_eq!(
            MotionSpeed::Fast.duration(),
            Some(Duration::from_millis(180))
        );
        assert_eq!(
            MotionSpeed::Smooth.duration(),
            Some(Duration::from_millis(360))
        );
    }

    #[test]
    fn workspace_pane_scope_is_isolated_by_default() {
        assert_eq!(WorkspacePaneMode::default(), WorkspacePaneMode::Workspace);
        let current = HashSet::from(["shell-a".to_string(), "shell-b".to_string()]);
        let same = HashSet::from(["shell-b".to_string(), "shell-a".to_string()]);
        let different = HashSet::from(["shell-c".to_string()]);

        assert!(!workspace_open_replaces_panes(
            WorkspacePaneMode::Workspace,
            &current,
            &same
        ));
        assert!(workspace_open_replaces_panes(
            WorkspacePaneMode::Workspace,
            &current,
            &different
        ));
        assert!(!workspace_open_replaces_panes(
            WorkspacePaneMode::Mixed,
            &current,
            &different
        ));
    }

    #[test]
    fn workspace_reordering_moves_to_either_side_and_keeps_new_items_last() {
        let mut order = vec!["alpha".into(), "bravo".into(), "charlie".into()];
        assert!(reorder_workspace(&mut order, "charlie", "alpha", false));
        assert_eq!(order, ["charlie", "alpha", "bravo"]);
        assert!(reorder_workspace(&mut order, "charlie", "bravo", true));
        assert_eq!(order, ["alpha", "bravo", "charlie"]);

        let mut overview = BoomuxOverview {
            workspaces: ["new-workspace", "bravo", "alpha", "charlie"]
                .into_iter()
                .map(|id| terminal::WorkspaceChoice {
                    id: id.into(),
                    name: id.into(),
                    shells: Vec::new(),
                    agent_count: 0,
                })
                .collect(),
            ..BoomuxOverview::default()
        };
        reconcile_workspace_order(&mut order, &mut overview);
        assert_eq!(order.last().map(String::as_str), Some("new-workspace"));
        assert_eq!(
            overview
                .workspaces
                .iter()
                .map(|workspace| workspace.id.as_str())
                .collect::<Vec<_>>(),
            order.iter().map(String::as_str).collect::<Vec<_>>()
        );
    }

    #[test]
    fn workspace_drag_targets_both_directions_with_expanded_rows() {
        let workspace = |id: &str, shell_count: usize| terminal::WorkspaceChoice {
            id: id.into(),
            name: id.into(),
            shells: (0..shell_count)
                .map(|index| ShellChoice {
                    id: format!("{id}-shell-{index}"),
                    name: format!("shell-{index}"),
                    workspace_id: id.into(),
                    cwd: "/tmp".into(),
                    status: ShellStatus::Running,
                    run_id: None,
                    desktop_setup: false,
                })
                .collect(),
            agent_count: 0,
        };
        let overview = BoomuxOverview {
            workspaces: vec![
                workspace("alpha", 0),
                workspace("bravo", 2),
                workspace("charlie", 0),
            ],
            ..BoomuxOverview::default()
        };
        let expanded = HashSet::from(["bravo".to_string()]);

        assert_eq!(
            sidebar_workspace_drop_target(&overview, &expanded, PaneLayoutMode::Tiled, -10.0),
            Some(("alpha".into(), false))
        );
        assert_eq!(
            sidebar_workspace_drop_target(&overview, &expanded, PaneLayoutMode::Tiled, 70.0),
            Some(("bravo".into(), false))
        );
        assert_eq!(
            sidebar_workspace_drop_target(&overview, &expanded, PaneLayoutMode::Tiled, 175.0),
            Some(("bravo".into(), true))
        );
        assert_eq!(
            sidebar_workspace_drop_target(&overview, &expanded, PaneLayoutMode::Tiled, 500.0),
            Some(("charlie".into(), true))
        );

        let offsets = sidebar_workspace_offsets(&overview, &expanded, PaneLayoutMode::Tiled);
        assert_eq!(offsets["alpha"], 0.0);
        assert_eq!(offsets["bravo"], SIDEBAR_WORKSPACE_HEADER_HEIGHT);
        assert_eq!(offsets["charlie"], 182.0);
    }

    #[test]
    fn opening_one_shell_only_replaces_panes_from_another_workspace() {
        let same_workspace = HashSet::from(["workspace-a".to_string()]);
        let another_workspace = HashSet::from(["workspace-b".to_string()]);

        assert!(!shell_open_replaces_panes(
            WorkspacePaneMode::Workspace,
            &same_workspace,
            "workspace-a"
        ));
        assert!(shell_open_replaces_panes(
            WorkspacePaneMode::Workspace,
            &another_workspace,
            "workspace-a"
        ));
        assert!(!shell_open_replaces_panes(
            WorkspacePaneMode::Mixed,
            &another_workspace,
            "workspace-a"
        ));
    }

    #[test]
    fn minimized_shell_identity_survives_workspace_navigation() {
        let minimized = HashSet::from(["shell-b".to_string()]);

        assert!(!shell_is_minimized(&minimized, "shell-a"));
        assert!(shell_is_minimized(&minimized, "shell-b"));
    }

    #[test]
    fn workspace_mode_collapses_other_sidebar_workspaces() {
        let mut expanded = HashSet::from(["workspace-a".to_string(), "workspace-b".to_string()]);
        reveal_opened_workspace(WorkspacePaneMode::Workspace, &mut expanded, "workspace-c");
        assert_eq!(expanded, HashSet::from(["workspace-c".to_string()]));

        reveal_opened_workspace(WorkspacePaneMode::Mixed, &mut expanded, "workspace-d");
        assert_eq!(
            expanded,
            HashSet::from(["workspace-c".to_string(), "workspace-d".to_string()])
        );
    }

    #[test]
    fn sidebar_distinguishes_focused_open_and_minimized_shells() {
        assert_eq!(shell_pane_presence(true, true), ShellPanePresence::Focused);
        assert_eq!(shell_pane_presence(false, true), ShellPanePresence::Open);
        assert_eq!(
            shell_pane_presence(false, false),
            ShellPanePresence::Minimized
        );
        assert_eq!(ShellPanePresence::Minimized.label(), "minimized");
    }

    #[test]
    fn scrollbar_drag_applies_pointer_delta_to_the_start_offset() {
        assert_eq!(scrollbar_offset_from_drag(40, 80, -50.0, 80.0), 0);
        assert_eq!(scrollbar_offset_from_drag(40, 80, 20.0, 80.0), 60);
        assert_eq!(scrollbar_offset_from_drag(40, 80, 50.0, 80.0), 80);
    }

    #[test]
    fn scrollbar_fade_reaches_the_correct_visibility_endpoints() {
        assert_eq!(scrollbar_fade_opacity(true, 0.0), 0.0);
        assert_eq!(scrollbar_fade_opacity(true, 1.0), 1.0);
        assert_eq!(scrollbar_fade_opacity(false, 0.0), 1.0);
        assert_eq!(scrollbar_fade_opacity(false, 1.0), 0.0);
        assert!(SCROLLBAR_FADE_IN_DURATION < SCROLLBAR_FADE_OUT_DURATION);
    }

    #[test]
    fn mixed_corners_are_stable_and_include_square_and_curved_edges() {
        let corners = pane_corner_radii(42, PaneCornerStyle::Mixed);
        assert_eq!(corners, pane_corner_radii(42, PaneCornerStyle::Mixed));
        assert!(corners.contains(&0.0));
        assert!(corners.iter().any(|radius| *radius > 0.0));
        assert!(
            corners
                .iter()
                .all(|radius| [0.0, 3.0, 7.0, 12.0, 18.0].contains(radius))
        );
    }

    #[test]
    fn focus_highlight_strength_blends_between_inactive_and_active_colors() {
        assert_eq!(
            blend_rgb(
                theme::resolve_legacy(0x313244),
                theme::resolve_legacy(0xcba6f7),
                0
            ),
            0x313244
        );
        assert_eq!(
            blend_rgb(
                theme::resolve_legacy(0x313244),
                theme::resolve_legacy(0xcba6f7),
                100
            ),
            0xcba6f7
        );
        assert_eq!(
            blend_rgb(
                theme::resolve_legacy(0x000000),
                theme::resolve_legacy(0xffffff),
                50
            ),
            0x808080
        );
        assert_eq!(
            blend_rgb(
                theme::resolve_legacy(0x000000),
                theme::resolve_legacy(0xffffff),
                200
            ),
            0xffffff
        );
    }

    #[test]
    fn resizing_respects_minimum_and_panel_edge() {
        let small = dragged_bounds(
            pane(),
            PointerOperation::Resize,
            (-900.0, -900.0),
            (800.0, 600.0),
        );
        assert_eq!(small.width, MIN_FLOAT_WIDTH);
        assert_eq!(small.height, MIN_FLOAT_HEIGHT);

        let large = dragged_bounds(
            pane(),
            PointerOperation::Resize,
            (900.0, 900.0),
            (800.0, 600.0),
        );
        assert_eq!(large.width, 700.0);
        assert_eq!(large.height, 520.0);
    }

    #[test]
    fn tiled_resize_applies_both_pointer_axes_at_once() {
        let mut layout = Node::pane(1);
        layout.split(1, 2, Axis::Horizontal);
        layout.split(2, 3, Axis::Vertical);

        let resized = resized_tiled_layout(layout, 3, (120.0, -120.0), (1200.0, 600.0));
        let pane = resized
            .rects()
            .into_iter()
            .find(|(id, _)| *id == 3)
            .unwrap()
            .1;

        assert!((pane.width - 0.4).abs() < f32::EPSILON);
        assert!((pane.y - 0.3).abs() < f32::EPSILON);
        assert!((pane.height - 0.7).abs() < f32::EPSILON);
    }

    #[test]
    fn drop_side_selects_insertion_axis_and_order() {
        let mut layout = Node::pane(1);
        layout.split(1, 2, Axis::Horizontal);

        assert_eq!(
            drop_placement(&layout, (0.05, 0.5)),
            Some((1, Axis::Horizontal, true))
        );
        assert_eq!(
            drop_placement(&layout, (0.75, 0.95)),
            Some((2, Axis::Vertical, false))
        );
    }

    fn sidebar_overview() -> BoomuxOverview {
        let shell = ShellChoice {
            id: "shell-1".into(),
            name: "lively-dolphin".into(),
            workspace_id: "workspace-1".into(),
            cwd: "/tmp".into(),
            status: ShellStatus::Running,
            run_id: Some("run-1".into()),
            desktop_setup: false,
        };
        BoomuxOverview {
            workspaces: vec![terminal::WorkspaceChoice {
                id: "workspace-1".into(),
                name: "boomux-desktop".into(),
                shells: vec![shell],
                agent_count: 1,
            }],
            agents: vec![AgentChoice {
                run_id: "run-1".into(),
                id: "agent-1".into(),
                shell_name: "lively-dolphin".into(),
                display_name: "lively-dolphin".into(),
                workspace: "boomux-desktop".into(),
                shell_id: "shell-1".into(),
                integration: "codex".into(),
                state: AgentState::Idle,
                updated_at_ms: 1,
                needs_attention: false,
                completed_attention: false,
                attention_revision: None,
            }],
            focused_shell_id: Some("shell-1".into()),
        }
    }

    #[test]
    fn setup_pane_cleanup_requires_finished_output_and_confirmed_shell_absence() {
        let mut overview = sidebar_overview();
        let mut shell = overview.workspaces[0].shells[0].clone();
        shell.desktop_setup = true;
        assert!(!super::setup_pane_was_removed(&shell, true, &overview));
        overview.workspaces[0].shells.clear();
        assert!(!super::setup_pane_was_removed(&shell, false, &overview));
        assert!(super::setup_pane_was_removed(&shell, true, &overview));
        shell.desktop_setup = false;
        assert!(!super::setup_pane_was_removed(&shell, true, &overview));
    }

    #[test]
    fn working_to_idle_agent_completion_remains_until_dismissed() {
        let mut previous = HashMap::from([("agent-1".into(), AgentState::Working)]);
        let mut completed = HashSet::new();
        let mut agents = sidebar_overview().agents;
        agents[0].state = AgentState::Idle;

        reconcile_completed_agents(&mut previous, &mut completed, &agents);
        assert!(completed.contains("agent-1"));

        reconcile_completed_agents(&mut previous, &mut completed, &agents);
        assert!(completed.contains("agent-1"));

        completed.remove("agent-1");
        reconcile_completed_agents(&mut previous, &mut completed, &agents);
        assert!(!completed.contains("agent-1"));
    }

    #[test]
    fn resumed_agent_clears_its_previous_completion() {
        let mut previous = HashMap::from([("agent-1".into(), AgentState::Idle)]);
        let mut completed = HashSet::from(["agent-1".into()]);
        let mut agents = sidebar_overview().agents;
        agents[0].state = AgentState::Working;

        reconcile_completed_agents(&mut previous, &mut completed, &agents);

        assert!(!completed.contains("agent-1"));
        assert_eq!(previous.get("agent-1"), Some(&AgentState::Working));
    }

    #[test]
    fn completion_tracking_is_bounded_to_agents_in_the_overview() {
        let mut previous = HashMap::from([("removed-agent".into(), AgentState::Working)]);
        let mut completed = HashSet::from(["removed-agent".into()]);

        reconcile_completed_agents(&mut previous, &mut completed, &[]);

        assert!(previous.is_empty());
        assert!(completed.is_empty());
    }

    #[test]
    fn sidebar_navigation_contains_only_visible_tree_rows() {
        let overview = sidebar_overview();
        let collapsed = visible_sidebar_items(&overview, &HashSet::new());
        assert_eq!(
            collapsed,
            vec![
                SidebarItem::Workspace("workspace-1".into()),
                SidebarItem::Agent {
                    agent_id: "agent-1".into(),
                    shell_id: "shell-1".into(),
                },
            ]
        );

        let expanded =
            visible_sidebar_items(&overview, &HashSet::from([String::from("workspace-1")]));
        assert!(matches!(
            &expanded[1],
            SidebarItem::Shell {
                workspace_id,
                shell_id,
            } if workspace_id == "workspace-1" && shell_id == "shell-1"
        ));
    }

    #[test]
    fn sidebar_selection_survives_refresh_by_identity() {
        let selected = SidebarItem::Agent {
            agent_id: "agent-1".into(),
            shell_id: "shell-1".into(),
        };
        let visible = vec![
            SidebarItem::Workspace("workspace-1".into()),
            selected.clone(),
        ];
        assert_eq!(
            reconciled_sidebar_item(Some(&selected), None, &visible),
            Some(selected)
        );
    }

    #[test]
    fn sidebar_selection_falls_back_to_preferred_visible_item() {
        let stale = SidebarItem::Workspace("removed".into());
        let preferred = SidebarItem::Workspace("workspace-1".into());
        let visible = vec![preferred.clone()];
        assert_eq!(
            reconciled_sidebar_item(Some(&stale), Some(&preferred), &visible),
            Some(preferred)
        );
    }

    #[test]
    fn sidebar_rows_resolve_their_exact_workspace_for_shell_creation() {
        let overview = sidebar_overview();
        for item in [
            SidebarItem::Workspace("workspace-1".into()),
            SidebarItem::Shell {
                workspace_id: "workspace-1".into(),
                shell_id: "shell-1".into(),
            },
            SidebarItem::Agent {
                agent_id: "agent-1".into(),
                shell_id: "shell-1".into(),
            },
        ] {
            assert_eq!(
                workspace_id_for_sidebar_item(&item, &overview).as_deref(),
                Some("workspace-1")
            );
        }

        assert_eq!(
            workspace_id_for_sidebar_item(
                &SidebarItem::Shell {
                    workspace_id: "workspace-1".into(),
                    shell_id: "stale-shell".into(),
                },
                &overview,
            ),
            None
        );
    }

    #[test]
    fn resource_names_reject_controls_and_have_a_fixed_bound() {
        let mut name = String::from("shell-");
        append_resource_name(&mut name, "one\n\ttwo");
        assert_eq!(name, "shell-onetwo");

        append_resource_name(&mut name, &"x".repeat(256));
        assert_eq!(name.chars().count(), 128);
    }

    #[test]
    fn help_catalog_covers_every_shortcut_section() {
        for section in [
            ShortcutSection::General,
            ShortcutSection::Navigation,
            ShortcutSection::Panes,
            ShortcutSection::Terminal,
            ShortcutSection::Sidebar,
        ] {
            assert!(
                HELP_SHORTCUTS
                    .iter()
                    .any(|shortcut| shortcut.section == section),
                "missing help shortcuts for {}",
                section.label()
            );
        }
    }
}
