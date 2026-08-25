use std::io::{self, Read, Write};
use std::path::PathBuf;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 46;
pub const MIN_PROTOCOL_VERSION: u32 = 6;
pub const MAX_CONTROL_FRAME: usize = 8 * 1024 * 1024;
pub const MAX_ATTACH_FRAME: usize = 1024 * 1024;

fn is_false(value: &bool) -> bool {
    !*value
}

macro_rules! define_protocol_features {
    ($($variant:ident => ($version:literal, $requirement:literal, [$($capability:literal),* $(,)?])),* $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum ProtocolFeature {
            $($variant),*
        }

        impl ProtocolFeature {
            pub const ALL: &'static [Self] = &[$(Self::$variant),*];

            pub const fn minimum_version(self) -> u32 {
                match self {
                    $(Self::$variant => $version),*
                }
            }

            pub const fn is_supported_by(self, negotiated_version: u32) -> bool {
                negotiated_version >= self.minimum_version()
            }

            pub const fn capability_names(self) -> &'static [&'static str] {
                match self {
                    $(Self::$variant => &[$($capability),*]),*
                }
            }

            pub const fn requirement(self) -> &'static str {
                match self {
                    $(Self::$variant => $requirement),*
                }
            }
        }
    };
}

define_protocol_features! {
    Baseline => (6, "request", [
        "typed_errors",
        "shell_run_identity",
        "rendered_scrollback",
        "graceful_live_handoff",
        "graceful_exited_handoff",
    ]),
    AtomicOutputReads => (7, "request", [
        "daemon_events",
        "reconnectable_event_cursors",
        "revision_aware_reads",
    ]),
    WorkspaceLaunchers => (8, "request", ["workspace_launchers"]),
    AgentInstances => (9, "request", ["run_scoped_agent_instances", "agent_authority_precedence"]),
    IdempotentAgentEnsure => (10, "request", ["protocol_10", "idempotent_agent_ensure"]),
    RestartableExitedShells => (11, "request", ["protocol_11", "restartable_exited_shells"]),
    InactiveAgentState => (12, "inactive agent state", [
        "protocol_12",
        "inactive_agent_state",
        "projected_agent_sessions",
    ]),
    DurableAgentCwd => (13, "request", ["protocol_13", "durable_session_source_context"]),
    RevisionAwareAgentWait => (14, "agent wait", ["protocol_14", "revision_aware_agent_wait"]),
    PersistentAgentAttention => (15, "agent attention acknowledgment", [
        "protocol_15",
        "persistent_agent_attention",
    ]),
    ClientEnvironment => (16, "client environment", ["protocol_16"]),
    RestartNotificationConfig => (17, "request", ["protocol_17"]),
    FocusedTerminal => (18, "request", ["protocol_18", "focused_terminal_following"]),
    WorkspaceDefaultCwd => (19, "request", ["protocol_19", "workspace_default_cwd"]),
    StructuredTerminalPreview => (20, "request", ["protocol_20", "structured_terminal_previews"]),
    FocusedTerminalRead => (21, "request", ["protocol_21", "focused_terminal_read"]),
    AgentSchedules => (22, "agent schedule management", [
        "protocol_22",
        "agent_schedule_management",
        "durable_agent_schedules",
    ]),
    ScheduledExecutions => (23, "scheduled execution dispatch", [
        "protocol_23",
        "scheduled_execution_dispatch",
        "scheduled_execution_cancellation",
        "schedule_owned_shells",
    ]),
    TimedScheduling => (24, "timed scheduling", [
        "protocol_24",
        "timed_schedule_dispatch",
        "scheduler_health",
        "bounded_scheduled_execution_concurrency",
    ]),
    ScheduledExecutionObservation => (25, "scheduled execution observation", [
        "protocol_25",
        "revision_aware_scheduled_execution_wait",
        "bounded_scheduled_execution_history",
        "scheduled_execution_notifications",
    ]),
    ExactRunAttachment => (26, "exact run attachment", [
        "protocol_26",
        "exact_run_attachment",
    ]),
    AgentScheduleEditing => (27, "agent schedule editing", [
        "protocol_27",
        "agent_schedule_editing",
    ]),
    NodeIdentity => (28, "Node identity", [
        "protocol_28",
        "stable_node_identity",
    ]),
    FederationChannel => (29, "federation daemon channel", [
        "protocol_29",
        "federation_daemon_channel",
    ]),
    NodeRekey => (30, "Node rekey", ["protocol_30", "node_rekey"]),
    NodeRegistration => (31, "Node registration management", [
        "protocol_31",
        "node_registration_management",
        "pinned_node_identity",
    ]),
    NodeProjectionSync => (32, "Node projection synchronization", [
        "protocol_32",
        "node_projection_sync",
        "bounded_remote_node_projections",
    ]),
    CombinedNodeSnapshot => (33, "combined Node snapshot", [
        "protocol_33",
        "combined_node_snapshot",
        "node_qualified_dashboard",
    ]),
    GuardedNodeRouting => (34, "guarded Node routing", [
        "protocol_34",
        "typed_exact_node_routing",
        "guarded_remote_management",
    ]),
    RemotePtyAttachment => (35, "remote PTY attachment", [
        "protocol_35",
        "remote_pty_attachment",
        "owner_environment_attachment",
    ]),
    NodeHostServices => (36, "Node host services", [
        "protocol_36",
        "typed_node_host_services",
        "remote_project_discovery",
        "remote_launcher_invocation",
        "remote_integration_management",
        "remote_agent_session_catalog",
        "remote_exact_session_resume",
    ]),
    RemoteSchedules => (37, "remote Schedule management", [
        "protocol_37",
        "remote_agent_schedule_management",
        "remote_scheduled_execution_observation",
    ]),
    GlobalWorkspaces => (38, "coordinated multi-Node Workspaces", [
        "protocol_38",
        "global_workspaces",
        "multi_node_workspace_placements",
        "guarded_workspace_adoption",
        "resumable_workspace_close",
    ]),
    QualifiedFocusedTerminal => (39, "Node-qualified focused terminal presentation", [
        "protocol_39",
        "qualified_focused_terminal",
    ]),
    RecoveredAgentPresentation => (40, "recovered Agent presentation", [
        "protocol_40",
        "recovered_agent_presentation",
    ]),
    CachedProjectionDismissal => (40, "cached projection dismissal", [
        "cached_projection_dismissal",
    ]),
    ObservedNodeHelperVersion => (41, "observed Node helper version", [
        "protocol_41",
        "observed_node_helper_version",
    ]),
    NodeUpgradeCoordination => (41, "Node upgrade coordination", [
        "node_upgrade_coordination",
    ]),
    OpenCodeSharedRuntimeClaims => (42, "OpenCode shared runtime claims", [
        "protocol_42",
        "opencode_shared_runtime_claims",
    ]),
    ClaudeRemoteControlBindings => (43, "Claude Remote Control bindings", [
        "protocol_43",
        "claude_remote_control_bindings",
    ]),
    CollaborativeExactRunAttachment => (44, "collaborative exact run attachment", [
        "protocol_44",
        "collaborative_exact_run_attachment",
    ]),
    KiroLaunchHolders => (45, "Kiro exact launch holders", [
        "protocol_45",
        "kiro_exact_launch_holders",
    ]),
    KiroStopIdle => (46, "Kiro Stop idle reporting", [
        "protocol_46",
        "kiro_stop_idle",
    ]),
}

pub const DEFAULT_SCHEDULED_EXECUTION_LIST_LIMIT: u16 = 100;
pub const MAX_SCHEDULED_EXECUTION_LIST_LIMIT: u16 = 1_000;
pub const MAX_SCHEDULED_EXECUTION_SCHEDULE_PROJECTIONS: u16 = 100;
pub const MAX_NODE_PROJECTION_EXECUTIONS: u16 = 1_000;
pub const MAX_NODE_PROJECTION_TRANSITIONS: u16 = 256;
pub const MAX_HOST_SERVICE_PROJECTS: usize = 2_000;
pub const MAX_HOST_SERVICE_WARNINGS: usize = 64;
pub const MAX_HOST_SERVICE_SESSIONS: usize = 1_000;
pub const MAX_CLAUDE_REMOTE_CONTROL_BINDINGS: usize = 4_096;
pub const MAX_CLAUDE_BRIDGE_SESSION_ID_BYTES: usize = 256;
pub const MAX_KIRO_LAUNCH_HOLDERS: usize = 256;
pub const MAX_KIRO_HOLDER_SESSIONS: usize = 16;
pub const MAX_KIRO_SESSION_ID_BYTES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostServiceIntegrationAction {
    Install,
    Uninstall,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostServiceOperation {
    DiscoverProjects,
    ResolveDirectory {
        path: PathBuf,
    },
    SuggestShellName {
        workspace_id: String,
    },
    InvokeLauncher {
        workspace_id: String,
        launcher_id: String,
    },
    IntegrationStatus {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        integration: Option<String>,
    },
    PreviewIntegrationMutation {
        action: HostServiceIntegrationAction,
        integrations: Vec<String>,
        force: bool,
    },
    CommitIntegrationMutation {
        preview_token: String,
    },
    VerifyIntegration {
        integration: String,
        shell_id: String,
        run_id: String,
    },
    ListAgentSessions {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace_id: Option<String>,
    },
    InspectAgentSession {
        session_id: String,
    },
    ResolveAgentSession {
        workspace_id: String,
        agent_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostProjectSnapshot {
    pub name: String,
    pub path: PathBuf,
    pub group: String,
    pub group_order: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostProjectDiscovery {
    pub roots_configured: bool,
    pub projects: Vec<HostProjectSnapshot>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostIntegrationStatus {
    pub name: String,
    pub display_name: String,
    pub package: String,
    pub validated_version: String,
    pub host_state: String,
    pub executable: Option<String>,
    pub version: Option<String>,
    pub compatibility: String,
    pub host_error: Option<String>,
    pub asset_state: String,
    pub path: Option<String>,
    pub asset_error: Option<String>,
    pub runtime_state: String,
    pub running_processes: usize,
    pub tracked_processes: usize,
    pub untracked_processes: usize,
    pub recommended_action: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostIntegrationPlan {
    pub name: String,
    pub current_state: String,
    pub action: String,
    pub path: String,
    pub restart_required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostIntegrationMutationPreview {
    pub token: String,
    pub action: HostServiceIntegrationAction,
    pub force: bool,
    pub plans: Vec<HostIntegrationPlan>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostIntegrationMutationResult {
    pub name: String,
    pub result: String,
    pub path: String,
    pub restart_required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostAgentSessionSummary {
    pub id: String,
    pub workspace_id: String,
    pub workspace_name: String,
    pub description: String,
    pub integration: String,
    pub external_session_id: Option<String>,
    pub state: AgentState,
    pub state_is_current: bool,
    pub started_at_ms: u64,
    pub last_at_ms: u64,
    pub occurrence_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostAgentSessionInspection {
    pub summary: HostAgentSessionSummary,
    pub source_cwd: Option<PathBuf>,
    pub occurrences: Vec<AgentInstanceSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostAgentSessionResumePlan {
    pub session_id: String,
    pub workspace_id: String,
    pub workspace_name: String,
    pub integration: String,
    pub cwd: PathBuf,
    pub argv: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostServiceResult {
    Projects {
        discovery: HostProjectDiscovery,
    },
    Directory {
        path: PathBuf,
    },
    ShellName {
        workspace_id: String,
        name: String,
    },
    LauncherInvoked {
        workspace_id: String,
        launcher_id: String,
    },
    IntegrationStatus {
        integrations: Vec<HostIntegrationStatus>,
    },
    IntegrationMutationPreview {
        preview: HostIntegrationMutationPreview,
    },
    IntegrationMutation {
        integrations: Vec<HostIntegrationMutationResult>,
    },
    IntegrationVerified {
        integration: String,
        shell_id: String,
        run_id: String,
        agents: Vec<AgentInstanceSnapshot>,
    },
    AgentSessions {
        sessions: Vec<HostAgentSessionSummary>,
    },
    AgentSession {
        session: HostAgentSessionInspection,
    },
    ResolvedAgentSession {
        session: HostAgentSessionSummary,
    },
}

pub fn protocol_capabilities() -> impl Iterator<Item = &'static str> {
    ProtocolFeature::ALL
        .iter()
        .copied()
        .filter(|feature| feature.is_supported_by(PROTOCOL_VERSION))
        .flat_map(ProtocolFeature::capability_names)
        .copied()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalColor {
    #[default]
    Default,
    Indexed(u8),
    Rgb {
        red: u8,
        green: u8,
        blue: u8,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalStyle {
    #[serde(default)]
    pub foreground: TerminalColor,
    #[serde(default)]
    pub background: TerminalColor,
    #[serde(default)]
    pub bold: bool,
    #[serde(default)]
    pub dim: bool,
    #[serde(default)]
    pub italic: bool,
    #[serde(default)]
    pub underline: bool,
    #[serde(default)]
    pub inverse: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalPreviewSpan {
    pub text: String,
    #[serde(default)]
    pub style: TerminalStyle,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalPreviewLine {
    pub spans: Vec<TerminalPreviewSpan>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalPreview {
    pub lines: Vec<TerminalPreviewLine>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope<T> {
    pub version: u32,
    pub message: T,
}

impl<T> Envelope<T> {
    pub fn new(message: T) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            message,
        }
    }

    pub fn with_version(version: u32, message: T) -> Self {
        Self { version, message }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub workspaces: Vec<WorkspaceSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused_terminal: Option<FocusedTerminalSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduler: Option<SchedulerHealth>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchedulerState {
    Active,
    Offline,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchedulerHealth {
    pub state: SchedulerState,
    pub max_concurrent: u16,
    pub active_executions: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FocusedTerminalSnapshot {
    pub revision: u64,
    pub workspace_id: String,
    pub shell_id: String,
    pub run_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualifiedFocusedTerminalSnapshot {
    pub revision: u64,
    pub shell: QualifiedIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeRegistrationSnapshot {
    pub alias: String,
    pub target: String,
    pub node_id: String,
    pub revision: u64,
    pub tombstone_epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualifiedIdentity {
    pub node_id: String,
    pub inner_id: String,
}

impl QualifiedIdentity {
    pub fn new(node_id: impl Into<String>, inner_id: impl Into<String>) -> Self {
        Self {
            node_id: node_id.into(),
            inner_id: inner_id.into(),
        }
    }
}

impl From<&str> for QualifiedIdentity {
    fn from(inner_id: &str) -> Self {
        Self::new("", inner_id)
    }
}

impl PartialEq<str> for QualifiedIdentity {
    fn eq(&self, other: &str) -> bool {
        self.inner_id == other
    }
}

impl PartialEq<&str> for QualifiedIdentity {
    fn eq(&self, other: &&str) -> bool {
        self.inner_id == *other
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeProjectionHealthCode {
    Unobserved,
    Online,
    Reconnecting,
    Stale,
    Unreachable,
    AuthenticationRequired,
    IdentityChanged,
    IdentityConflict,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeProjectionHealth {
    pub code: NodeProjectionHealthCode,
    pub stale: bool,
    pub cache_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_attempt_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_success_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_at_ms: Option<u64>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_helper_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeProjectionWorkspace {
    pub id: String,
    pub name: String,
    pub item_count: u32,
    pub attention_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeProjectionShell {
    pub id: String,
    pub workspace_id: String,
    pub name: String,
    pub owner: ShellOwner,
    pub status: ShellStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovered_agent_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeProjectionLauncher {
    pub id: String,
    pub workspace_id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeProjectionAttention {
    pub reason: AgentAttentionReason,
    pub observation_revision: u64,
    pub observed_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeProjectionAgent {
    pub id: String,
    pub workspace_id: String,
    pub shell_id: String,
    pub run_id: String,
    pub name: String,
    pub integration: String,
    pub state: AgentState,
    pub observation_revision: u64,
    pub observed_at_ms: u64,
    pub started_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention: Option<NodeProjectionAttention>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeProjectionSchedule {
    pub id: String,
    pub workspace_id: String,
    pub name: String,
    pub integration: String,
    pub state: AgentScheduleState,
    pub trigger: AgentScheduleTrigger,
    pub revision: u64,
    pub prompt_revision: u64,
    pub trigger_revision: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_occurrence: Option<ScheduledOccurrence>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeProjectionExecution {
    pub id: String,
    pub workspace_id: String,
    pub schedule_id: String,
    pub revision: u64,
    pub state: ScheduledExecutionState,
    pub dispatch_kind: ScheduledExecutionDispatchKind,
    pub schedule_revision: u64,
    pub prompt_revision: u64,
    pub trigger_revision: u64,
    pub requested_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduled_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<ScheduledExecutionReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<ScheduledExecutionOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeProjectionSnapshot {
    pub node_id: String,
    pub workspaces: Vec<NodeProjectionWorkspace>,
    pub shells: Vec<NodeProjectionShell>,
    pub launchers: Vec<NodeProjectionLauncher>,
    pub agents: Vec<NodeProjectionAgent>,
    pub schedules: Vec<NodeProjectionSchedule>,
    pub executions: Vec<NodeProjectionExecution>,
    pub executions_truncated: bool,
    pub scheduler: SchedulerHealth,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "transition", rename_all = "snake_case", deny_unknown_fields)]
pub enum NodeProjectionTransitionKind {
    Workspace {
        workspace_id: String,
    },
    Shell {
        workspace_id: String,
        shell_id: String,
    },
    Launcher {
        workspace_id: String,
        launcher_id: String,
    },
    Agent {
        workspace_id: String,
        agent_id: String,
        revision: u64,
    },
    Schedule {
        workspace_id: String,
        schedule_id: String,
        revision: Option<u64>,
    },
    Execution {
        workspace_id: String,
        execution_id: String,
        revision: u64,
    },
    HandoffCompleted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeProjectionTransition {
    pub event_id: u64,
    pub at_ms: u64,
    pub kind: NodeProjectionTransitionKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeProjectionSyncMode {
    Baseline,
    Resumed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeProjectionSync {
    pub mode: NodeProjectionSyncMode,
    pub cursor: EventCursor,
    pub projection: NodeProjectionSnapshot,
    pub transitions: Vec<NodeProjectionTransition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CombinedNodeSnapshot {
    pub nodes: Vec<CombinedNode>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspaces: Vec<GlobalWorkspaceSnapshot>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub external_workspaces: Vec<ExternalWorkspaceSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused_terminal: Option<QualifiedFocusedTerminalSnapshot>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspacePlacementState {
    Active,
    ClosePending,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspacePlacementSnapshot {
    pub node_id: String,
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_workspace_name: Option<String>,
    pub owner_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_cwd: Option<PathBuf>,
    pub state: WorkspacePlacementState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GlobalWorkspaceSnapshot {
    pub id: String,
    pub revision: u64,
    pub name: String,
    pub closing: bool,
    pub placements: Vec<WorkspacePlacementSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalWorkspaceSnapshot {
    pub identity: QualifiedIdentity,
    pub revision: u64,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_cwd: Option<PathBuf>,
    pub available: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspacePlacementResult {
    pub node_id: String,
    pub workspace_id: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GlobalWorkspaceOperationResult {
    pub workspace: GlobalWorkspaceSnapshot,
    pub placements: Vec<WorkspacePlacementResult>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CombinedNode {
    pub node_id: String,
    pub alias: String,
    pub local: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration_revision: Option<u64>,
    pub health: NodeProjectionHealthCode,
    pub current: bool,
    pub stale: bool,
    pub observed_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_protocol_version: Option<u32>,
    #[serde(default)]
    pub observed_capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_helper_version: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub workspace_owner_eligible: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_owner_unavailable_reason: Option<String>,
    pub scheduler: SchedulerHealth,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_snapshot: Option<Snapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_projection: Option<NodeProjectionSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    pub id: String,
    #[serde(default)]
    pub revision: u64,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_cwd: Option<PathBuf>,
    pub shells: Vec<ShellSnapshot>,
    #[serde(default)]
    pub launchers: Vec<WorkspaceLauncherSnapshot>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<AgentInstanceSnapshot>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub schedules: Vec<AgentScheduleSnapshot>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentScheduleState {
    #[default]
    Paused,
    Enabled,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentScheduleSession {
    #[default]
    Fresh,
    Continue {
        external_session_id: String,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentScheduleOverlapPolicy {
    #[default]
    Skip,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentScheduleTrigger {
    pub cron: String,
    pub timezone: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentScheduleSpec {
    pub name: String,
    pub cwd: PathBuf,
    pub integration: String,
    pub prompt: String,
    #[serde(default)]
    pub session: AgentScheduleSession,
    pub trigger: AgentScheduleTrigger,
    #[serde(default)]
    pub state: AgentScheduleState,
    #[serde(default)]
    pub overlap_policy: AgentScheduleOverlapPolicy,
}

impl std::fmt::Debug for AgentScheduleSpec {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentScheduleSpec")
            .field("name", &self.name)
            .field("cwd", &self.cwd)
            .field("integration", &self.integration)
            .field("prompt", &"<redacted>")
            .field("session", &self.session)
            .field("trigger", &self.trigger)
            .field("state", &self.state)
            .field("overlap_policy", &self.overlap_policy)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentScheduleUpdate {
    pub name: String,
    pub prompt: String,
    pub trigger: AgentScheduleTrigger,
}

impl std::fmt::Debug for AgentScheduleUpdate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentScheduleUpdate")
            .field("name", &self.name)
            .field("prompt", &"<redacted>")
            .field("trigger", &self.trigger)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentScheduleSnapshot {
    pub id: String,
    pub workspace_id: String,
    pub name: String,
    pub cwd: PathBuf,
    pub integration: String,
    #[serde(default)]
    pub session: AgentScheduleSession,
    pub trigger: AgentScheduleTrigger,
    #[serde(default)]
    pub state: AgentScheduleState,
    #[serde(default)]
    pub overlap_policy: AgentScheduleOverlapPolicy,
    pub revision: u64,
    pub prompt_revision: u64,
    pub trigger_revision: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub evaluation_frontier_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_shell_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_occurrence: Option<ScheduledOccurrence>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledOccurrence {
    pub trigger_revision: u64,
    pub scheduled_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledExecutionScheduleProjection {
    pub schedule_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_occurrence: Option<ScheduledOccurrence>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentScheduleInspection {
    pub schedule: AgentScheduleSnapshot,
    pub prompt: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduledExecutionState {
    Skipped,
    Claimed,
    Starting,
    Active,
    DispatchFailed,
    Exited,
    Cancelled,
    Interrupted,
}

impl ScheduledExecutionState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Skipped
                | Self::DispatchFailed
                | Self::Exited
                | Self::Cancelled
                | Self::Interrupted
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduledExecutionDispatchKind {
    Manual,
    Timed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduledExecutionReason {
    Overlap,
    ActiveSession,
    WorkspaceCapacity,
    GlobalCapacity,
    Missed,
    PausedRace,
    InvalidTarget,
    RunnerStartFailed,
    HostSpawnFailed,
    CancelledByUser,
    ColdDaemonRecovery,
    RunnerExitedWithoutReport,
    DaemonShutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScheduledExecutionOutcome {
    ExitCode { code: i32 },
    Signal { signal: i32 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledExecutionSnapshot {
    pub id: String,
    pub workspace_id: String,
    pub schedule_id: String,
    #[serde(default)]
    pub revision: u64,
    pub state: ScheduledExecutionState,
    pub dispatch_kind: ScheduledExecutionDispatchKind,
    pub dispatch_key: String,
    pub schedule_revision: u64,
    pub prompt_revision: u64,
    pub trigger_revision: u64,
    pub requested_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduled_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coalesced_through_ms: Option<u64>,
    pub started_at_ms: Option<u64>,
    pub ended_at_ms: Option<u64>,
    pub cwd: PathBuf,
    pub integration: String,
    pub session: AgentScheduleSession,
    pub reason: Option<ScheduledExecutionReason>,
    pub outcome: Option<ScheduledExecutionOutcome>,
    pub shell_id: Option<String>,
    pub run_id: Option<String>,
    pub agent_id: Option<String>,
    pub external_session_id: Option<String>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledExecutionClaim {
    pub execution: ScheduledExecutionSnapshot,
    pub prompt: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ScheduledRunnerCapability(String);

impl ScheduledRunnerCapability {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ScheduledRunnerCapability {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("<redacted>")
    }
}

impl std::fmt::Debug for ScheduledExecutionClaim {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ScheduledExecutionClaim")
            .field("execution", &self.execution)
            .field("prompt", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum ScheduledRunnerResult {
    Active,
    SpawnFailed,
    Exited { outcome: ScheduledExecutionOutcome },
}

impl std::fmt::Debug for AgentScheduleInspection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentScheduleInspection")
            .field("schedule", &self.schedule)
            .field("prompt", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Unknown,
    Working,
    Blocked,
    Idle,
    Inactive,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentAuthority {
    LifecycleIntegration,
    ProcessAdapter,
    TerminalHeuristic,
    DaemonLifecycle,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentReport {
    pub state: AgentState,
    pub authority: AgentAuthority,
    pub evidence: String,
    pub confidence: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRegistrationSpec {
    pub name: String,
    pub integration: String,
    pub external_session_id: Option<String>,
    pub report: AgentReport,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentObservationSnapshot {
    pub revision: u64,
    pub state: AgentState,
    pub authority: AgentAuthority,
    pub evidence: String,
    pub confidence: u8,
    pub observed_at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentAttentionReason {
    Blocked,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentAttentionSnapshot {
    pub reason: AgentAttentionReason,
    pub observation: AgentObservationSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInstanceSnapshot {
    pub id: String,
    pub workspace_id: String,
    pub shell_id: String,
    pub run_id: String,
    pub name: String,
    pub integration: String,
    pub external_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    pub started_at_ms: u64,
    pub ended_at_ms: Option<u64>,
    pub observation: AgentObservationSnapshot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention: Option<AgentAttentionSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenCodeSharedRuntimeSnapshot {
    pub generation_id: String,
    pub url: String,
    pub port: u16,
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenCodeSessionClaimSnapshot {
    pub generation_id: String,
    pub claim_id: String,
    pub holder_id: String,
    pub root_session_id: String,
    pub workspace_id: String,
    pub shell_id: String,
    pub run_id: String,
    pub agent_id: String,
    pub holder_count: u32,
    pub holder_expires_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeRemoteControlBindingSnapshot {
    pub agent_id: String,
    pub shell_id: String,
    pub run_id: String,
    pub bridge_session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceLauncherSpec {
    pub name: String,
    pub command: Vec<String>,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceLauncherSnapshot {
    pub id: String,
    #[serde(default)]
    pub revision: u64,
    pub workspace_id: String,
    pub name: String,
    pub command: Vec<String>,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellSnapshot {
    pub id: String,
    #[serde(default)]
    pub revision: u64,
    pub workspace_id: String,
    pub name: String,
    pub cwd: PathBuf,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    #[serde(default)]
    pub owner: ShellOwner,
    pub status: ShellStatus,
    #[serde(default)]
    pub run: Option<ShellRunSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovered_agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground_process: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ShellOwner {
    #[default]
    User,
    Schedule {
        schedule_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellStatus {
    Pending,
    Running,
    Exited { code: Option<u32> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellRunSnapshot {
    pub id: String,
    pub generation: u64,
    pub started_at_ms: u64,
    pub ended_at_ms: Option<u64>,
    pub exit_reason: Option<ShellRunExitReason>,
    pub output_revision: u64,
    pub environment_has_run_id: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum ShellRunExitReason {
    Exited { code: Option<u32> },
    Terminated,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidArgument,
    NotFound,
    AlreadyExists,
    Busy,
    DaemonStopping,
    ShellStartFailed,
    PersistenceFailed,
    Timeout,
    UnsupportedVersion,
    CursorExpired,
    RunChanged,
    RevisionAhead,
    IdempotencyExpired,
    NodeIdentityUnavailable,
    NodeRegistrationUnavailable,
    NodeIdentityChanged,
    AmbiguousTarget,
    RevisionChanged,
    OutcomeUnknown,
    Internal,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum RoutedOperation {
    CreateWorkspaceShell {
        workspace_id: String,
        workspace_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default_cwd: Option<PathBuf>,
        shell_id: String,
        shell: ShellSpec,
    },
    CreateWorkspaceLauncher {
        workspace_id: String,
        workspace_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default_cwd: Option<PathBuf>,
        launcher_id: String,
        spec: WorkspaceLauncherSpec,
    },
    CreateWorkspaceAgentSchedule {
        workspace_id: String,
        workspace_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default_cwd: Option<PathBuf>,
        schedule_id: String,
        spec: AgentScheduleSpec,
    },
    CreateAgentSchedule {
        workspace_id: String,
        spec: AgentScheduleSpec,
    },
    GetWorkspace {
        workspace_id: String,
    },
    GetShell {
        shell_id: String,
    },
    GetLauncher {
        launcher_id: String,
    },
    GetAgent {
        agent_id: String,
    },
    GetAgentSchedule {
        schedule_id: String,
    },
    GetScheduledExecution {
        execution_id: String,
    },
    ListScheduledExecutions {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schedule_id: Option<String>,
        limit: u16,
    },
    WaitScheduledExecution {
        execution_id: String,
        after_revision: u64,
        #[serde(default)]
        wait_ms: u32,
    },
    RenameWorkspace {
        workspace_id: String,
        name: String,
        expected_revision: u64,
    },
    RenameShell {
        shell_id: String,
        name: String,
        expected_revision: u64,
    },
    RenameLauncher {
        launcher_id: String,
        name: String,
        expected_revision: u64,
    },
    CloseWorkspace {
        workspace_id: String,
        expected_revision: u64,
    },
    CloseShell {
        shell_id: String,
        expected_revision: u64,
    },
    RestartShell {
        shell_id: String,
        expected_revision: u64,
        expected_run_id: String,
    },
    RemoveLauncher {
        launcher_id: String,
        expected_revision: u64,
    },
    PauseAgentSchedule {
        schedule_id: String,
        expected_revision: u64,
    },
    ResumeAgentSchedule {
        schedule_id: String,
        expected_revision: u64,
    },
    UpdateAgentSchedule {
        schedule_id: String,
        expected_revision: u64,
        update: AgentScheduleUpdate,
    },
    RemoveAgentSchedule {
        schedule_id: String,
        expected_revision: u64,
    },
    RunAgentSchedule {
        schedule_id: String,
        dispatch_key: String,
    },
    CancelScheduledExecution {
        execution_id: String,
        expected_revision: u64,
    },
    AcknowledgeAgentAttention {
        agent_id: String,
        observation_revision: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutedGuard {
    None,
    ExactId,
    ResourceRevision,
    ResourceRevisionAndRun,
    ScheduleRevision,
    ExecutionRevision,
    DispatchKey,
    AttentionObservationRevision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutedAmbiguity {
    RetrySameRequest,
    ReadAndProve,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoutedOperationClass {
    pub guard: RoutedGuard,
    pub ambiguity: RoutedAmbiguity,
}

impl RoutedOperation {
    pub const fn classification(&self) -> RoutedOperationClass {
        use RoutedAmbiguity::{ReadAndProve, RetrySameRequest};
        use RoutedGuard::{
            AttentionObservationRevision, DispatchKey, ExactId, ExecutionRevision, None,
            ResourceRevision, ResourceRevisionAndRun, ScheduleRevision,
        };
        match self {
            Self::CreateWorkspaceShell { .. }
            | Self::CreateWorkspaceLauncher { .. }
            | Self::CreateWorkspaceAgentSchedule { .. } => RoutedOperationClass {
                guard: ExactId,
                ambiguity: RetrySameRequest,
            },
            Self::CreateAgentSchedule { .. } => RoutedOperationClass {
                guard: None,
                ambiguity: ReadAndProve,
            },
            Self::GetWorkspace { .. }
            | Self::GetShell { .. }
            | Self::GetLauncher { .. }
            | Self::GetAgent { .. }
            | Self::GetAgentSchedule { .. }
            | Self::GetScheduledExecution { .. }
            | Self::ListScheduledExecutions { .. }
            | Self::WaitScheduledExecution { .. } => RoutedOperationClass {
                guard: ExactId,
                ambiguity: RetrySameRequest,
            },
            Self::RenameWorkspace { .. }
            | Self::RenameShell { .. }
            | Self::RenameLauncher { .. }
            | Self::CloseWorkspace { .. }
            | Self::CloseShell { .. }
            | Self::RemoveLauncher { .. } => RoutedOperationClass {
                guard: ResourceRevision,
                ambiguity: ReadAndProve,
            },
            Self::RestartShell { .. } => RoutedOperationClass {
                guard: ResourceRevisionAndRun,
                ambiguity: ReadAndProve,
            },
            Self::PauseAgentSchedule { .. }
            | Self::ResumeAgentSchedule { .. }
            | Self::UpdateAgentSchedule { .. }
            | Self::RemoveAgentSchedule { .. } => RoutedOperationClass {
                guard: ScheduleRevision,
                ambiguity: ReadAndProve,
            },
            Self::RunAgentSchedule { .. } => RoutedOperationClass {
                guard: DispatchKey,
                ambiguity: RetrySameRequest,
            },
            Self::CancelScheduledExecution { .. } => RoutedOperationClass {
                guard: ExecutionRevision,
                ambiguity: ReadAndProve,
            },
            Self::AcknowledgeAgentAttention { .. } => RoutedOperationClass {
                guard: AttentionObservationRevision,
                ambiguity: RetrySameRequest,
            },
        }
    }

    pub const fn is_retryable(&self) -> bool {
        matches!(
            self.classification().ambiguity,
            RoutedAmbiguity::RetrySameRequest
        )
    }

    pub fn ambiguity_probe(&self) -> Option<Request> {
        match self {
            Self::CreateWorkspaceShell { .. }
            | Self::CreateWorkspaceLauncher { .. }
            | Self::CreateWorkspaceAgentSchedule { .. } => None,
            Self::RenameWorkspace { workspace_id, .. }
            | Self::CloseWorkspace { workspace_id, .. } => Some(Request::GetWorkspace {
                workspace_id: workspace_id.clone(),
            }),
            Self::RenameShell { shell_id, .. }
            | Self::CloseShell { shell_id, .. }
            | Self::RestartShell { shell_id, .. } => Some(Request::GetShell {
                shell_id: shell_id.clone(),
            }),
            Self::RenameLauncher { launcher_id, .. } | Self::RemoveLauncher { launcher_id, .. } => {
                Some(Request::GetLauncher {
                    launcher_id: launcher_id.clone(),
                })
            }
            Self::PauseAgentSchedule { schedule_id, .. }
            | Self::ResumeAgentSchedule { schedule_id, .. }
            | Self::UpdateAgentSchedule { schedule_id, .. }
            | Self::RemoveAgentSchedule { schedule_id, .. } => Some(Request::GetAgentSchedule {
                schedule_id: schedule_id.clone(),
            }),
            Self::CancelScheduledExecution { execution_id, .. } => {
                Some(Request::GetScheduledExecution {
                    execution_id: execution_id.clone(),
                })
            }
            _ => None,
        }
    }

    pub fn owner_request(&self) -> Request {
        match self.clone() {
            Self::CreateWorkspaceShell {
                workspace_id,
                workspace_name,
                default_cwd,
                shell_id,
                shell,
            } => Request::CreateWorkspaceShell {
                workspace_id,
                workspace_name,
                default_cwd,
                shell_id,
                shell,
            },
            Self::CreateWorkspaceLauncher {
                workspace_id,
                workspace_name,
                default_cwd,
                launcher_id,
                spec,
            } => Request::CreateWorkspaceLauncher {
                workspace_id,
                workspace_name,
                default_cwd,
                launcher_id,
                spec,
            },
            Self::CreateWorkspaceAgentSchedule {
                workspace_id,
                workspace_name,
                default_cwd,
                schedule_id,
                spec,
            } => Request::CreateWorkspaceAgentSchedule {
                workspace_id,
                workspace_name,
                default_cwd,
                schedule_id,
                spec,
            },
            Self::CreateAgentSchedule { workspace_id, spec } => {
                Request::CreateAgentSchedule { workspace_id, spec }
            }
            Self::GetWorkspace { workspace_id } => Request::GetWorkspace { workspace_id },
            Self::GetShell { shell_id } => Request::GetShell { shell_id },
            Self::GetLauncher { launcher_id } => Request::GetLauncher { launcher_id },
            Self::GetAgent { agent_id } => Request::GetAgent { agent_id },
            Self::GetAgentSchedule { schedule_id } => Request::GetAgentSchedule { schedule_id },
            Self::GetScheduledExecution { execution_id } => {
                Request::GetScheduledExecution { execution_id }
            }
            Self::ListScheduledExecutions {
                workspace_id,
                schedule_id,
                limit,
            } => Request::ListScheduledExecutions {
                workspace_id,
                schedule_id,
                limit: Some(limit),
            },
            Self::WaitScheduledExecution {
                execution_id,
                after_revision,
                wait_ms,
            } => Request::WaitScheduledExecution {
                execution_id,
                after_revision,
                wait_ms,
            },
            Self::RenameWorkspace {
                workspace_id,
                name,
                expected_revision,
            } => Request::GuardedRenameWorkspace {
                workspace_id,
                name,
                expected_revision,
            },
            Self::RenameShell {
                shell_id,
                name,
                expected_revision,
            } => Request::GuardedRenameShell {
                shell_id,
                name,
                expected_revision,
            },
            Self::RenameLauncher {
                launcher_id,
                name,
                expected_revision,
            } => Request::GuardedRenameLauncher {
                launcher_id,
                name,
                expected_revision,
            },
            Self::CloseWorkspace {
                workspace_id,
                expected_revision,
            } => Request::GuardedCloseWorkspace {
                workspace_id,
                expected_revision,
            },
            Self::CloseShell {
                shell_id,
                expected_revision,
            } => Request::GuardedCloseShell {
                shell_id,
                expected_revision,
            },
            Self::RestartShell {
                shell_id,
                expected_revision,
                expected_run_id,
            } => Request::GuardedRestartShell {
                shell_id,
                expected_revision,
                expected_run_id,
            },
            Self::RemoveLauncher {
                launcher_id,
                expected_revision,
            } => Request::GuardedRemoveLauncher {
                launcher_id,
                expected_revision,
            },
            Self::PauseAgentSchedule {
                schedule_id,
                expected_revision,
            } => Request::GuardedPauseAgentSchedule {
                schedule_id,
                expected_revision,
            },
            Self::ResumeAgentSchedule {
                schedule_id,
                expected_revision,
            } => Request::GuardedResumeAgentSchedule {
                schedule_id,
                expected_revision,
            },
            Self::UpdateAgentSchedule {
                schedule_id,
                expected_revision,
                update,
            } => Request::UpdateAgentSchedule {
                schedule_id,
                expected_revision,
                update,
            },
            Self::RemoveAgentSchedule {
                schedule_id,
                expected_revision,
            } => Request::GuardedRemoveAgentSchedule {
                schedule_id,
                expected_revision,
            },
            Self::RunAgentSchedule {
                schedule_id,
                dispatch_key,
            } => Request::RunAgentSchedule {
                schedule_id,
                dispatch_key,
            },
            Self::CancelScheduledExecution {
                execution_id,
                expected_revision,
            } => Request::GuardedCancelScheduledExecution {
                execution_id,
                expected_revision,
            },
            Self::AcknowledgeAgentAttention {
                agent_id,
                observation_revision,
            } => Request::AcknowledgeAgentAttention {
                agent_id,
                observation_revision,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventCursor {
    pub stream_id: String,
    pub event_id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonEvent {
    pub id: u64,
    pub at_ms: u64,
    #[serde(flatten)]
    pub kind: DaemonEventKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum DaemonEventKind {
    WorkspaceCreated {
        workspace_id: String,
        name: String,
    },
    WorkspaceRenamed {
        workspace_id: String,
        name: String,
    },
    WorkspaceClosed {
        workspace_id: String,
    },
    ShellCreated {
        workspace_id: String,
        shell_id: String,
        name: String,
    },
    ShellRenamed {
        workspace_id: String,
        shell_id: String,
        name: String,
    },
    ShellClosed {
        workspace_id: Option<String>,
        shell_id: String,
    },
    LauncherCreated {
        workspace_id: String,
        launcher_id: String,
        name: String,
    },
    LauncherRenamed {
        workspace_id: String,
        launcher_id: String,
        name: String,
    },
    LauncherRemoved {
        workspace_id: String,
        launcher_id: String,
    },
    RunStarted {
        workspace_id: String,
        shell_id: String,
        run: ShellRunSnapshot,
    },
    OutputChanged {
        workspace_id: String,
        shell_id: String,
        run_id: String,
        output_revision: u64,
    },
    RunExited {
        workspace_id: String,
        shell_id: String,
        run: ShellRunSnapshot,
    },
    AgentRegistered {
        workspace_id: String,
        shell_id: String,
        agent: AgentInstanceSnapshot,
    },
    AgentStateChanged {
        workspace_id: String,
        shell_id: String,
        agent: AgentInstanceSnapshot,
    },
    AgentCompleted {
        workspace_id: String,
        shell_id: String,
        agent: AgentInstanceSnapshot,
    },
    AgentAttentionAcknowledged {
        workspace_id: String,
        shell_id: String,
        agent: AgentInstanceSnapshot,
    },
    AgentScheduleCreated {
        workspace_id: String,
        schedule: AgentScheduleSnapshot,
    },
    AgentSchedulePaused {
        workspace_id: String,
        schedule: AgentScheduleSnapshot,
    },
    AgentScheduleResumed {
        workspace_id: String,
        schedule: AgentScheduleSnapshot,
    },
    AgentScheduleUpdated {
        workspace_id: String,
        schedule: AgentScheduleSnapshot,
    },
    AgentScheduleRemoved {
        workspace_id: String,
        schedule_id: String,
    },
    ScheduledExecutionCreated {
        workspace_id: String,
        execution: ScheduledExecutionSnapshot,
    },
    ScheduledExecutionChanged {
        workspace_id: String,
        execution: ScheduledExecutionSnapshot,
    },
    NodeProjectionChanged {
        node_id: String,
        cache_generation: u64,
    },
    FocusedTerminalPresentationChanged,
    HandoffCompleted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalProfile {
    pub term: Option<String>,
    pub colorterm: Option<String>,
    pub term_program: Option<String>,
    pub term_program_version: Option<String>,
    pub rows: u16,
    pub cols: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnixEnvironment {
    pub variables: Vec<UnixEnvironmentVariable>,
}

impl std::fmt::Debug for UnixEnvironment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UnixEnvironment")
            .field(
                "variables",
                &format_args!("<redacted: {}>", self.variables.len()),
            )
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnixEnvironmentVariable {
    pub name: Vec<u8>,
    pub value: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationDeliveryConfig {
    pub desktop_enabled: bool,
    pub sound_enabled: bool,
    pub blocked: bool,
    pub completed: bool,
    #[serde(default)]
    pub scheduled_dispatch_failed: bool,
    #[serde(default)]
    pub scheduled_interrupted: bool,
    pub blocked_sound: String,
    pub completed_sound: String,
    #[serde(default = "default_scheduled_dispatch_failed_sound")]
    pub scheduled_dispatch_failed_sound: String,
    #[serde(default = "default_scheduled_interrupted_sound")]
    pub scheduled_interrupted_sound: String,
    #[serde(default = "default_true")]
    pub resume_agents: bool,
    #[serde(default)]
    pub persist_terminal_history: bool,
    #[serde(default = "default_max_scheduled_execution_concurrency")]
    pub max_scheduled_execution_concurrency: u16,
}

impl Default for NotificationDeliveryConfig {
    fn default() -> Self {
        Self {
            desktop_enabled: false,
            sound_enabled: false,
            blocked: true,
            completed: true,
            scheduled_dispatch_failed: false,
            scheduled_interrupted: false,
            blocked_sound: "message-new-instant".into(),
            completed_sound: "complete".into(),
            scheduled_dispatch_failed_sound: default_scheduled_dispatch_failed_sound(),
            scheduled_interrupted_sound: default_scheduled_interrupted_sound(),
            resume_agents: true,
            persist_terminal_history: false,
            max_scheduled_execution_concurrency: 4,
        }
    }
}

fn default_max_scheduled_execution_concurrency() -> u16 {
    4
}

fn default_true() -> bool {
    true
}

fn default_scheduled_dispatch_failed_sound() -> String {
    "dialog-warning".into()
}

fn default_scheduled_interrupted_sound() -> String {
    "dialog-warning".into()
}

impl std::fmt::Debug for UnixEnvironmentVariable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("UnixEnvironmentVariable(<redacted>)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellSpec {
    pub name: String,
    #[serde(default)]
    pub command: Vec<String>,
    pub cwd: PathBuf,
}

impl ShellSpec {
    pub fn login(name: impl Into<String>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            command: Vec::new(),
            cwd: cwd.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "snake_case")]
pub enum Request {
    Ping,
    GetNodeIdentity,
    OpenFederationChannel,
    RekeyNode {
        expected_node_id: String,
    },
    AddNodeRegistration {
        alias: String,
        target: String,
        node_id: String,
    },
    ListNodeRegistrations,
    GetNodeRegistration {
        selector: String,
    },
    RenameNodeRegistration {
        selector: String,
        alias: String,
        expected_revision: u64,
    },
    RetargetNodeRegistration {
        selector: String,
        target: String,
        node_id: String,
        expected_revision: u64,
    },
    ForgetNodeRegistration {
        selector: String,
    },
    BeginNodeUpgradeMaintenance {
        selector: String,
        expected_revision: u64,
    },
    FinishNodeUpgradeMaintenance {
        node_id: String,
        token: String,
    },
    RenewNodeUpgradeMaintenance {
        node_id: String,
        token: String,
    },
    SyncNodeProjection {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after: Option<EventCursor>,
        #[serde(default)]
        wait_ms: u32,
    },
    GetNodeProjectionHealth {
        selector: String,
    },
    ForceNodeProjectionRefresh {
        selector: String,
    },
    DismissNodeProjectionShell {
        node_id: String,
        shell_id: String,
    },
    RestoreDismissedNodeProjectionShells {
        node_id: String,
    },
    GetCombinedNodeSnapshot {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selector: Option<String>,
    },
    CreateGlobalWorkspace {
        name: String,
    },
    AdoptNodeWorkspace {
        identity: QualifiedIdentity,
        expected_revision: u64,
    },
    LinkNodeWorkspace {
        global_workspace_id: String,
        expected_global_revision: u64,
        identity: QualifiedIdentity,
        expected_owner_revision: u64,
    },
    RenameGlobalWorkspace {
        workspace_id: String,
        expected_revision: u64,
        name: String,
    },
    OpenGlobalWorkspace {
        workspace_id: String,
        expected_revision: u64,
    },
    CloseGlobalWorkspace {
        workspace_id: String,
        expected_revision: u64,
    },
    RetryGlobalWorkspaceClose {
        workspace_id: String,
    },
    CreateGlobalWorkspaceShell {
        operation_id: String,
        global_workspace_id: String,
        expected_global_revision: u64,
        node_id: String,
        owner_workspace_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default_cwd: Option<PathBuf>,
        shell_id: String,
        shell: ShellSpec,
    },
    CreateGlobalWorkspaceWithShell {
        operation_id: String,
        global_workspace_id: String,
        name: String,
        node_id: String,
        owner_workspace_id: String,
        default_cwd: PathBuf,
        shell_id: String,
        shell: ShellSpec,
    },
    CreateGlobalWorkspaceLauncher {
        operation_id: String,
        global_workspace_id: String,
        expected_global_revision: u64,
        node_id: String,
        owner_workspace_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default_cwd: Option<PathBuf>,
        launcher_id: String,
        spec: WorkspaceLauncherSpec,
    },
    CreateGlobalWorkspaceAgentSchedule {
        operation_id: String,
        global_workspace_id: String,
        expected_global_revision: u64,
        node_id: String,
        owner_workspace_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default_cwd: Option<PathBuf>,
        schedule_id: String,
        spec: AgentScheduleSpec,
    },
    RouteNodeOperation {
        node_id: String,
        operation: RoutedOperation,
    },
    HostService {
        operation: HostServiceOperation,
    },
    RouteNodeHostService {
        node_id: String,
        operation: HostServiceOperation,
    },
    Restart,
    RestartWithNotificationConfig {
        notifications: NotificationDeliveryConfig,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        environment: Option<UnixEnvironment>,
    },
    Shutdown,
    Snapshot,
    GetFocusedTerminal,
    GetWorkspace {
        workspace_id: String,
    },
    GetShell {
        shell_id: String,
    },
    GetLauncher {
        launcher_id: String,
    },
    GetAgent {
        agent_id: String,
    },
    GetAgentSchedule {
        schedule_id: String,
    },
    ListScheduledExecutions {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schedule_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u16>,
    },
    GetScheduledExecution {
        execution_id: String,
    },
    WaitScheduledExecution {
        execution_id: String,
        after_revision: u64,
        #[serde(default)]
        wait_ms: u32,
    },
    WaitAgent {
        agent_id: String,
        after_revision: u64,
        #[serde(default)]
        wait_ms: u32,
    },
    CreateWorkspace {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default_cwd: Option<PathBuf>,
        shells: Vec<ShellSpec>,
    },
    CreateWorkspaceShell {
        workspace_id: String,
        workspace_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default_cwd: Option<PathBuf>,
        shell_id: String,
        shell: ShellSpec,
    },
    CreateWorkspaceLauncher {
        workspace_id: String,
        workspace_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default_cwd: Option<PathBuf>,
        launcher_id: String,
        spec: WorkspaceLauncherSpec,
    },
    CreateWorkspaceAgentSchedule {
        workspace_id: String,
        workspace_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default_cwd: Option<PathBuf>,
        schedule_id: String,
        spec: AgentScheduleSpec,
    },
    CreateShell {
        #[serde(default)]
        workspace_id: Option<String>,
        shell: ShellSpec,
    },
    CreateLauncher {
        workspace_id: String,
        spec: WorkspaceLauncherSpec,
    },
    CreateAgentSchedule {
        workspace_id: String,
        spec: AgentScheduleSpec,
    },
    UpdateAgentSchedule {
        schedule_id: String,
        expected_revision: u64,
        update: AgentScheduleUpdate,
    },
    RunAgentSchedule {
        schedule_id: String,
        dispatch_key: String,
    },
    CancelScheduledExecution {
        execution_id: String,
    },
    GuardedCancelScheduledExecution {
        execution_id: String,
        expected_revision: u64,
    },
    ResolveScheduledExecutionClaim {
        schedule_id: String,
        shell_id: String,
        run_id: String,
        runner_token: ScheduledRunnerCapability,
    },
    ReportScheduledRunner {
        execution_id: String,
        shell_id: String,
        run_id: String,
        runner_token: ScheduledRunnerCapability,
        result: ScheduledRunnerResult,
    },
    RegisterAgent {
        shell_id: String,
        run_id: String,
        spec: AgentRegistrationSpec,
    },
    EnsureAgent {
        shell_id: String,
        run_id: String,
        spec: AgentRegistrationSpec,
    },
    AcquireKiroLaunchHolder {
        pid: u32,
        shell_id: String,
        run_id: String,
    },
    ReportKiroHook {
        holder_id: String,
        session_id: String,
        report: AgentReport,
    },
    ReleaseKiroLaunchHolder {
        holder_id: String,
    },
    EnsureOpenCodeSharedRuntime {
        port: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        environment: Option<UnixEnvironment>,
    },
    GetOpenCodeSharedRuntime,
    EnsureOpenCodeSessionClaim {
        generation_id: String,
        holder_id: String,
        root_session_id: String,
        shell_id: String,
        run_id: String,
        spec: AgentRegistrationSpec,
    },
    ReleaseOpenCodeSessionClaim {
        generation_id: String,
        holder_id: String,
        claim_id: String,
    },
    ResolveOpenCodeSessionClaim {
        generation_id: String,
        root_session_id: String,
    },
    ReportClaimedOpenCodeAgent {
        generation_id: String,
        root_session_id: String,
        report: AgentReport,
    },
    SetClaudeRemoteControlBinding {
        agent_id: String,
        shell_id: String,
        run_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bridge_session_id: Option<String>,
    },
    GetClaudeRemoteControlBinding {
        agent_id: String,
        shell_id: String,
        run_id: String,
    },
    ReportAgent {
        agent_id: String,
        run_id: String,
        report: AgentReport,
    },
    AcknowledgeAgentAttention {
        agent_id: String,
        observation_revision: u64,
    },
    ReadShell {
        shell_id: String,
        max_bytes: usize,
    },
    ReadShellPreview {
        shell_id: String,
        max_bytes: usize,
        max_lines: u16,
    },
    ReadShellAt {
        shell_id: String,
        max_bytes: usize,
        #[serde(default)]
        run_id: Option<String>,
        #[serde(default)]
        after_revision: Option<u64>,
        #[serde(default)]
        wait_ms: u32,
    },
    Events {
        #[serde(default)]
        after: Option<EventCursor>,
        #[serde(default = "default_event_limit")]
        limit: u16,
        #[serde(default)]
        wait_ms: u32,
    },
    RenameWorkspace {
        workspace_id: String,
        name: String,
    },
    GuardedRenameWorkspace {
        workspace_id: String,
        name: String,
        expected_revision: u64,
    },
    RenameShell {
        shell_id: String,
        name: String,
    },
    GuardedRenameShell {
        shell_id: String,
        name: String,
        expected_revision: u64,
    },
    RenameLauncher {
        launcher_id: String,
        name: String,
    },
    GuardedRenameLauncher {
        launcher_id: String,
        name: String,
        expected_revision: u64,
    },
    CloseWorkspace {
        workspace_id: String,
    },
    GuardedCloseWorkspace {
        workspace_id: String,
        expected_revision: u64,
    },
    CloseShell {
        shell_id: String,
    },
    GuardedCloseShell {
        shell_id: String,
        expected_revision: u64,
    },
    RestartShell {
        shell_id: String,
    },
    GuardedRestartShell {
        shell_id: String,
        expected_revision: u64,
        expected_run_id: String,
    },
    RemoveLauncher {
        launcher_id: String,
    },
    GuardedRemoveLauncher {
        launcher_id: String,
        expected_revision: u64,
    },
    PauseAgentSchedule {
        schedule_id: String,
    },
    GuardedPauseAgentSchedule {
        schedule_id: String,
        expected_revision: u64,
    },
    ResumeAgentSchedule {
        schedule_id: String,
    },
    GuardedResumeAgentSchedule {
        schedule_id: String,
        expected_revision: u64,
    },
    RemoveAgentSchedule {
        schedule_id: String,
    },
    GuardedRemoveAgentSchedule {
        schedule_id: String,
        expected_revision: u64,
    },
    Attach {
        shell_id: String,
        takeover: bool,
        #[serde(default)]
        restart_exited: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_run_id: Option<String>,
        profile: TerminalProfile,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        environment: Option<UnixEnvironment>,
        #[serde(default, skip_serializing_if = "is_false")]
        owner_environment: bool,
    },
    AttachCollaborative {
        shell_id: String,
        expected_run_id: String,
        profile: TerminalProfile,
    },
    AttachNode {
        identity: QualifiedIdentity,
        takeover: bool,
        #[serde(default)]
        restart_exited: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_run_id: Option<String>,
        profile: TerminalProfile,
    },
    ResumeAgentSession {
        session_id: String,
        profile: TerminalProfile,
    },
    ResumeNodeAgentSession {
        node_id: String,
        session_id: String,
        profile: TerminalProfile,
    },
}

impl Request {
    pub fn minimum_protocol_version(&self) -> u32 {
        self.required_feature()
            .map(ProtocolFeature::minimum_version)
            .unwrap_or(MIN_PROTOCOL_VERSION)
    }

    pub fn required_feature(&self) -> Option<ProtocolFeature> {
        match self {
            Self::GetNodeIdentity => Some(ProtocolFeature::NodeIdentity),
            Self::OpenFederationChannel => Some(ProtocolFeature::FederationChannel),
            Self::RekeyNode { .. } => Some(ProtocolFeature::NodeRekey),
            Self::AddNodeRegistration { .. }
            | Self::ListNodeRegistrations
            | Self::GetNodeRegistration { .. }
            | Self::RenameNodeRegistration { .. }
            | Self::RetargetNodeRegistration { .. }
            | Self::ForgetNodeRegistration { .. } => Some(ProtocolFeature::NodeRegistration),
            Self::BeginNodeUpgradeMaintenance { .. }
            | Self::FinishNodeUpgradeMaintenance { .. }
            | Self::RenewNodeUpgradeMaintenance { .. } => {
                Some(ProtocolFeature::NodeUpgradeCoordination)
            }
            Self::SyncNodeProjection { .. } | Self::GetNodeProjectionHealth { .. } => {
                Some(ProtocolFeature::NodeProjectionSync)
            }
            Self::GetCombinedNodeSnapshot { .. } => Some(ProtocolFeature::CombinedNodeSnapshot),
            Self::ForceNodeProjectionRefresh { .. } => Some(ProtocolFeature::GlobalWorkspaces),
            Self::DismissNodeProjectionShell { .. }
            | Self::RestoreDismissedNodeProjectionShells { .. } => {
                Some(ProtocolFeature::CachedProjectionDismissal)
            }
            Self::CreateGlobalWorkspace { .. }
            | Self::AdoptNodeWorkspace { .. }
            | Self::LinkNodeWorkspace { .. }
            | Self::RenameGlobalWorkspace { .. }
            | Self::OpenGlobalWorkspace { .. }
            | Self::CloseGlobalWorkspace { .. }
            | Self::RetryGlobalWorkspaceClose { .. }
            | Self::CreateGlobalWorkspaceShell { .. }
            | Self::CreateGlobalWorkspaceWithShell { .. }
            | Self::CreateGlobalWorkspaceLauncher { .. }
            | Self::CreateGlobalWorkspaceAgentSchedule { .. } => {
                Some(ProtocolFeature::GlobalWorkspaces)
            }
            Self::CreateWorkspaceShell { .. }
            | Self::CreateWorkspaceLauncher { .. }
            | Self::CreateWorkspaceAgentSchedule { .. }
            | Self::RouteNodeOperation {
                operation:
                    RoutedOperation::CreateWorkspaceShell { .. }
                    | RoutedOperation::CreateWorkspaceLauncher { .. }
                    | RoutedOperation::CreateWorkspaceAgentSchedule { .. },
                ..
            } => Some(ProtocolFeature::GlobalWorkspaces),
            Self::RouteNodeOperation {
                operation:
                    RoutedOperation::CreateAgentSchedule { .. }
                    | RoutedOperation::ListScheduledExecutions { .. }
                    | RoutedOperation::WaitScheduledExecution { .. },
                ..
            } => Some(ProtocolFeature::RemoteSchedules),
            Self::RouteNodeOperation { .. }
            | Self::GuardedCancelScheduledExecution { .. }
            | Self::GuardedRenameWorkspace { .. }
            | Self::GuardedRenameShell { .. }
            | Self::GuardedRenameLauncher { .. }
            | Self::GuardedCloseWorkspace { .. }
            | Self::GuardedCloseShell { .. }
            | Self::GuardedRestartShell { .. }
            | Self::GuardedRemoveLauncher { .. }
            | Self::GuardedPauseAgentSchedule { .. }
            | Self::GuardedResumeAgentSchedule { .. }
            | Self::GuardedRemoveAgentSchedule { .. } => Some(ProtocolFeature::GuardedNodeRouting),
            Self::HostService { .. }
            | Self::RouteNodeHostService { .. }
            | Self::ResumeAgentSession { .. }
            | Self::ResumeNodeAgentSession { .. } => Some(ProtocolFeature::NodeHostServices),
            Self::AttachCollaborative { .. } => {
                Some(ProtocolFeature::CollaborativeExactRunAttachment)
            }
            Self::AttachNode { .. }
            | Self::Attach {
                owner_environment: true,
                ..
            } => Some(ProtocolFeature::RemotePtyAttachment),
            Self::WaitScheduledExecution { .. } => {
                Some(ProtocolFeature::ScheduledExecutionObservation)
            }
            Self::ListScheduledExecutions { .. }
            | Self::GetScheduledExecution { .. }
            | Self::RunAgentSchedule { .. }
            | Self::CancelScheduledExecution { .. }
            | Self::ResolveScheduledExecutionClaim { .. }
            | Self::ReportScheduledRunner { .. } => Some(ProtocolFeature::ScheduledExecutions),
            Self::UpdateAgentSchedule { .. } => Some(ProtocolFeature::AgentScheduleEditing),
            Self::CreateAgentSchedule { .. }
            | Self::GetAgentSchedule { .. }
            | Self::PauseAgentSchedule { .. }
            | Self::ResumeAgentSchedule { .. }
            | Self::RemoveAgentSchedule { .. } => Some(ProtocolFeature::AgentSchedules),
            Self::ReadShellPreview { .. } => Some(ProtocolFeature::StructuredTerminalPreview),
            Self::GetFocusedTerminal => Some(ProtocolFeature::FocusedTerminalRead),
            Self::CreateWorkspace {
                default_cwd: Some(_),
                ..
            }
            | Self::CreateShell {
                workspace_id: None, ..
            } => Some(ProtocolFeature::WorkspaceDefaultCwd),
            Self::RestartWithNotificationConfig { notifications, .. }
                if notifications.scheduled_dispatch_failed
                    || notifications.scheduled_interrupted =>
            {
                Some(ProtocolFeature::ScheduledExecutionObservation)
            }
            Self::RestartWithNotificationConfig { notifications, .. }
                if notifications.max_scheduled_execution_concurrency != 4 =>
            {
                Some(ProtocolFeature::TimedScheduling)
            }
            Self::RestartWithNotificationConfig {
                environment: Some(_),
                ..
            } => Some(ProtocolFeature::ScheduledExecutions),
            Self::RestartWithNotificationConfig { .. } => {
                Some(ProtocolFeature::RestartNotificationConfig)
            }
            Self::Attach {
                expected_run_id: Some(_),
                ..
            } => Some(ProtocolFeature::ExactRunAttachment),
            Self::Attach {
                environment: Some(_),
                ..
            } => Some(ProtocolFeature::ClientEnvironment),
            Self::AcknowledgeAgentAttention { .. } => {
                Some(ProtocolFeature::PersistentAgentAttention)
            }
            Self::WaitAgent { .. } => Some(ProtocolFeature::RevisionAwareAgentWait),
            Self::RegisterAgent { spec, .. } | Self::EnsureAgent { spec, .. }
                if spec.report.state == AgentState::Inactive =>
            {
                Some(ProtocolFeature::InactiveAgentState)
            }
            Self::ReportAgent { report, .. } if report.state == AgentState::Inactive => {
                Some(ProtocolFeature::InactiveAgentState)
            }
            Self::RestartShell { .. }
            | Self::Attach {
                restart_exited: true,
                ..
            } => Some(ProtocolFeature::RestartableExitedShells),
            Self::EnsureAgent { .. } => Some(ProtocolFeature::IdempotentAgentEnsure),
            Self::EnsureOpenCodeSharedRuntime { .. }
            | Self::GetOpenCodeSharedRuntime
            | Self::EnsureOpenCodeSessionClaim { .. }
            | Self::ReleaseOpenCodeSessionClaim { .. }
            | Self::ResolveOpenCodeSessionClaim { .. }
            | Self::ReportClaimedOpenCodeAgent { .. } => {
                Some(ProtocolFeature::OpenCodeSharedRuntimeClaims)
            }
            Self::SetClaudeRemoteControlBinding { .. }
            | Self::GetClaudeRemoteControlBinding { .. } => {
                Some(ProtocolFeature::ClaudeRemoteControlBindings)
            }
            Self::ReportKiroHook { report, .. } if report.state == AgentState::Idle => {
                Some(ProtocolFeature::KiroStopIdle)
            }
            Self::AcquireKiroLaunchHolder { .. }
            | Self::ReportKiroHook { .. }
            | Self::ReleaseKiroLaunchHolder { .. } => Some(ProtocolFeature::KiroLaunchHolders),
            Self::GetAgent { .. } | Self::RegisterAgent { .. } | Self::ReportAgent { .. } => {
                Some(ProtocolFeature::AgentInstances)
            }
            Self::GetLauncher { .. }
            | Self::CreateLauncher { .. }
            | Self::RenameLauncher { .. }
            | Self::RemoveLauncher { .. } => Some(ProtocolFeature::WorkspaceLaunchers),
            Self::ReadShellAt { .. } | Self::Events { .. } => {
                Some(ProtocolFeature::AtomicOutputReads)
            }
            Self::Ping
            | Self::Restart
            | Self::Shutdown
            | Self::Snapshot
            | Self::GetWorkspace { .. }
            | Self::GetShell { .. }
            | Self::CreateWorkspace { .. }
            | Self::CreateShell { .. }
            | Self::ReadShell { .. }
            | Self::RenameWorkspace { .. }
            | Self::RenameShell { .. }
            | Self::CloseWorkspace { .. }
            | Self::CloseShell { .. }
            | Self::Attach { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "response", rename_all = "snake_case")]
pub enum Response {
    Pong,
    NodeIdentity {
        node_id: String,
    },
    FederationChannel {
        node_id: String,
    },
    NodeRegistration {
        registration: NodeRegistrationSnapshot,
    },
    NodeRegistrations {
        registrations: Vec<NodeRegistrationSnapshot>,
    },
    NodeUpgradeMaintenance {
        registration: NodeRegistrationSnapshot,
        token: String,
    },
    NodeProjectionSync {
        sync: NodeProjectionSync,
    },
    NodeProjectionHealth {
        health: NodeProjectionHealth,
    },
    CombinedNodeSnapshot {
        snapshot: CombinedNodeSnapshot,
    },
    GlobalWorkspace {
        workspace: GlobalWorkspaceSnapshot,
    },
    GlobalWorkspaceOperation {
        result: GlobalWorkspaceOperationResult,
    },
    GlobalWorkspaceResource {
        workspace: GlobalWorkspaceSnapshot,
        resource: RoutedOperationResult,
    },
    RoutedNodeOperation {
        result: RoutedOperationResult,
    },
    HostService {
        result: HostServiceResult,
    },
    Snapshot {
        snapshot: Snapshot,
    },
    FocusedTerminal {
        focused_terminal: Option<FocusedTerminalSnapshot>,
    },
    Workspace {
        workspace: WorkspaceSnapshot,
    },
    Shell {
        shell: ShellSnapshot,
    },
    Launcher {
        launcher: WorkspaceLauncherSnapshot,
    },
    Agent {
        agent: AgentInstanceSnapshot,
    },
    KiroLaunchHolder {
        holder_id: String,
    },
    KiroLaunchHolderReleased {
        released: bool,
    },
    OpenCodeSharedRuntime {
        runtime: Option<OpenCodeSharedRuntimeSnapshot>,
    },
    OpenCodeSessionClaim {
        claim: OpenCodeSessionClaimSnapshot,
        agent: AgentInstanceSnapshot,
    },
    OpenCodeSessionClaimReleased {
        released: bool,
    },
    ClaudeRemoteControlBinding {
        binding: Option<ClaudeRemoteControlBindingSnapshot>,
    },
    AgentSchedule {
        schedule: AgentScheduleSnapshot,
    },
    AgentScheduleInspection {
        inspection: AgentScheduleInspection,
    },
    ScheduledExecution {
        execution: ScheduledExecutionSnapshot,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        next_occurrence: Option<ScheduledOccurrence>,
    },
    ScheduledExecutions {
        executions: Vec<ScheduledExecutionSnapshot>,
        #[serde(default)]
        limit: u16,
        #[serde(default)]
        truncated: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        schedules: Vec<ScheduledExecutionScheduleProjection>,
        #[serde(default)]
        schedule_limit: u16,
        #[serde(default)]
        schedules_truncated: bool,
    },
    ScheduledExecutionWait {
        execution: ScheduledExecutionSnapshot,
        changed: bool,
    },
    ScheduledExecutionClaim {
        claim: ScheduledExecutionClaim,
    },
    AgentWait {
        agent: AgentInstanceSnapshot,
        changed: bool,
    },
    AgentAttentionAcknowledged {
        agent: AgentInstanceSnapshot,
        changed: bool,
    },
    Output {
        bytes: Vec<u8>,
    },
    ShellPreview {
        preview: TerminalPreview,
    },
    OutputState {
        bytes: Vec<u8>,
        run_id: Option<String>,
        output_revision: Option<u64>,
        changed: bool,
        status: ShellStatus,
    },
    Events {
        stream_id: String,
        cursor: EventCursor,
        snapshot: Option<Snapshot>,
        events: Vec<DaemonEvent>,
    },
    Attached {
        token: String,
        reconstruction: Vec<u8>,
        warning: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<TerminalProfile>,
    },
    Ok,
    Error {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<ErrorCode>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum RoutedOperationResult {
    Workspace {
        workspace: WorkspaceSnapshot,
    },
    Shell {
        shell: ShellSnapshot,
    },
    Launcher {
        launcher: WorkspaceLauncherSnapshot,
    },
    Agent {
        agent: AgentInstanceSnapshot,
    },
    AgentSchedule {
        schedule: AgentScheduleSnapshot,
    },
    AgentScheduleInspection {
        inspection: AgentScheduleInspection,
    },
    ScheduledExecution {
        execution: ScheduledExecutionSnapshot,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        next_occurrence: Option<ScheduledOccurrence>,
    },
    ScheduledExecutions {
        executions: Vec<ScheduledExecutionSnapshot>,
        limit: u16,
        truncated: bool,
        schedules: Vec<ScheduledExecutionScheduleProjection>,
        schedule_limit: u16,
        schedules_truncated: bool,
    },
    ScheduledExecutionWait {
        execution: ScheduledExecutionSnapshot,
        changed: bool,
    },
    AgentAttentionAcknowledged {
        agent: AgentInstanceSnapshot,
        changed: bool,
    },
    Ok,
}

fn default_event_limit() -> u16 {
    256
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachFrame {
    Input(Vec<u8>),
    Output(Vec<u8>),
    Resize {
        rows: u16,
        cols: u16,
        pixel_width: u16,
        pixel_height: u16,
    },
    Detached,
    Reconnect,
    ReconnectAck,
    FocusGained,
}

impl AttachFrame {
    const INPUT: u8 = 1;
    const OUTPUT: u8 = 2;
    const RESIZE: u8 = 3;
    const DETACHED: u8 = 4;
    const RECONNECT: u8 = 5;
    const RECONNECT_ACK: u8 = 6;
    const FOCUS_GAINED: u8 = 7;

    pub fn write_to(&self, writer: &mut impl Write) -> io::Result<()> {
        let (kind, payload): (u8, &[u8]) = match self {
            Self::Input(bytes) => (Self::INPUT, bytes),
            Self::Output(bytes) => (Self::OUTPUT, bytes),
            Self::Resize {
                rows,
                cols,
                pixel_width,
                pixel_height,
            } => {
                writer.write_all(&[Self::RESIZE])?;
                writer.write_all(&8_u32.to_be_bytes())?;
                writer.write_all(&rows.to_be_bytes())?;
                writer.write_all(&cols.to_be_bytes())?;
                writer.write_all(&pixel_width.to_be_bytes())?;
                return writer.write_all(&pixel_height.to_be_bytes());
            }
            Self::Detached => (Self::DETACHED, &[]),
            Self::Reconnect => (Self::RECONNECT, &[]),
            Self::ReconnectAck => (Self::RECONNECT_ACK, &[]),
            Self::FocusGained => (Self::FOCUS_GAINED, &[]),
        };
        if payload.len() > MAX_ATTACH_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "attach frame too large",
            ));
        }
        writer.write_all(&[kind])?;
        writer.write_all(&(payload.len() as u32).to_be_bytes())?;
        writer.write_all(payload)
    }

    pub fn read_from(reader: &mut impl Read) -> io::Result<Self> {
        let mut kind = [0];
        reader.read_exact(&mut kind)?;
        let mut length = [0; 4];
        reader.read_exact(&mut length)?;
        let length = u32::from_be_bytes(length) as usize;
        if length > MAX_ATTACH_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "attach frame too large",
            ));
        }
        let mut payload = vec![0; length];
        reader.read_exact(&mut payload)?;
        match kind[0] {
            Self::INPUT => Ok(Self::Input(payload)),
            Self::OUTPUT => Ok(Self::Output(payload)),
            Self::RESIZE if payload.len() == 8 => Ok(Self::Resize {
                rows: u16::from_be_bytes([payload[0], payload[1]]),
                cols: u16::from_be_bytes([payload[2], payload[3]]),
                pixel_width: u16::from_be_bytes([payload[4], payload[5]]),
                pixel_height: u16::from_be_bytes([payload[6], payload[7]]),
            }),
            Self::DETACHED if payload.is_empty() => Ok(Self::Detached),
            Self::RECONNECT if payload.is_empty() => Ok(Self::Reconnect),
            Self::RECONNECT_ACK if payload.is_empty() => Ok(Self::ReconnectAck),
            Self::FOCUS_GAINED if payload.is_empty() => Ok(Self::FocusGained),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid attach frame",
            )),
        }
    }
}

pub fn write_message<T: Serialize>(writer: &mut impl Write, message: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec(message).map_err(io::Error::other)?;
    if bytes.len() > MAX_CONTROL_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "control frame too large",
        ));
    }
    writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
    writer.write_all(&bytes)
}

pub fn read_message<T: DeserializeOwned>(reader: &mut impl Read) -> io::Result<T> {
    let mut length = [0; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_CONTROL_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "control frame too large",
        ));
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_profile() -> TerminalProfile {
        TerminalProfile {
            term: None,
            colorterm: None,
            term_program: None,
            term_program_version: None,
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    fn test_report(state: AgentState) -> AgentReport {
        AgentReport {
            state,
            authority: AgentAuthority::LifecycleIntegration,
            evidence: "test".into(),
            confidence: 100,
        }
    }

    fn test_registration(state: AgentState) -> AgentRegistrationSpec {
        AgentRegistrationSpec {
            name: "agent".into(),
            integration: "test".into(),
            external_session_id: None,
            report: test_report(state),
        }
    }

    fn test_schedule() -> AgentScheduleSnapshot {
        AgentScheduleSnapshot {
            id: "schedule-1".into(),
            workspace_id: "w1".into(),
            name: "morning review".into(),
            cwd: "/tmp/project".into(),
            integration: "opencode".into(),
            session: AgentScheduleSession::Continue {
                external_session_id: "session-1".into(),
            },
            trigger: AgentScheduleTrigger {
                cron: "0 9 * * 1-5".into(),
                timezone: "America/New_York".into(),
            },
            state: AgentScheduleState::Enabled,
            overlap_policy: AgentScheduleOverlapPolicy::Skip,
            revision: 3,
            prompt_revision: 2,
            trigger_revision: 1,
            created_at_ms: 10,
            updated_at_ms: 20,
            evaluation_frontier_ms: 30,
            execution_shell_id: Some("shell-1".into()),
            next_occurrence: Some(ScheduledOccurrence {
                trigger_revision: 1,
                scheduled_at_ms: 60,
            }),
        }
    }

    fn test_execution() -> ScheduledExecutionSnapshot {
        ScheduledExecutionSnapshot {
            id: "execution-1".into(),
            workspace_id: "workspace-1".into(),
            schedule_id: "schedule-1".into(),
            revision: 4,
            state: ScheduledExecutionState::Exited,
            dispatch_kind: ScheduledExecutionDispatchKind::Manual,
            dispatch_key: "dispatch-1".into(),
            schedule_revision: 3,
            prompt_revision: 2,
            trigger_revision: 1,
            requested_at_ms: 10,
            scheduled_at_ms: None,
            coalesced_through_ms: None,
            started_at_ms: Some(11),
            ended_at_ms: Some(12),
            cwd: "/tmp/project".into(),
            integration: "opencode".into(),
            session: AgentScheduleSession::Fresh,
            reason: None,
            outcome: Some(ScheduledExecutionOutcome::ExitCode { code: 0 }),
            shell_id: Some("shell-1".into()),
            run_id: Some("run-1".into()),
            agent_id: None,
            external_session_id: None,
        }
    }

    #[test]
    fn scheduled_execution_observation_messages_round_trip_and_list_defaults_are_bounded() {
        let legacy: Request = serde_json::from_str(
            r#"{"request":"list_scheduled_executions","workspace_id":null,"schedule_id":null}"#,
        )
        .unwrap();
        assert!(matches!(
            legacy,
            Request::ListScheduledExecutions { limit: None, .. }
        ));

        let wait = Request::WaitScheduledExecution {
            execution_id: "execution-1".into(),
            after_revision: 3,
            wait_ms: 30_000,
        };
        assert_eq!(
            serde_json::from_value::<Request>(serde_json::to_value(&wait).unwrap()).unwrap(),
            wait
        );
        assert_eq!(
            wait.required_feature(),
            Some(ProtocolFeature::ScheduledExecutionObservation)
        );

        for response in [
            Response::ScheduledExecutions {
                executions: vec![test_execution()],
                limit: 100,
                truncated: true,
                schedules: vec![ScheduledExecutionScheduleProjection {
                    schedule_id: "schedule-1".into(),
                    next_occurrence: Some(ScheduledOccurrence {
                        trigger_revision: 1,
                        scheduled_at_ms: 100,
                    }),
                }],
                schedule_limit: 100,
                schedules_truncated: false,
            },
            Response::ScheduledExecutionWait {
                execution: test_execution(),
                changed: true,
            },
        ] {
            assert_eq!(
                serde_json::from_value::<Response>(serde_json::to_value(&response).unwrap())
                    .unwrap(),
                response
            );
        }
    }

    #[derive(Deserialize)]
    struct ProtocolSixShellSnapshot {
        id: String,
        status: ShellStatus,
    }

    #[derive(Deserialize)]
    #[serde(tag = "response", rename_all = "snake_case")]
    enum LegacyResponse {
        Error { message: String },
    }

    #[test]
    fn control_frame_round_trips() {
        let value = Envelope::new(Request::Attach {
            shell_id: "s1".into(),
            takeover: false,
            restart_exited: true,
            expected_run_id: None,
            profile: TerminalProfile {
                term: Some("xterm-256color".into()),
                colorterm: Some("truecolor".into()),
                term_program: Some("test".into()),
                term_program_version: Some("1".into()),
                rows: 24,
                cols: 80,
                pixel_width: 800,
                pixel_height: 600,
            },
            environment: Some(UnixEnvironment {
                variables: vec![UnixEnvironmentVariable {
                    name: b"NON_UTF8".to_vec(),
                    value: vec![0xff, 0xfe],
                }],
            }),
            owner_environment: false,
        });
        let mut bytes = Vec::new();
        write_message(&mut bytes, &value).unwrap();
        assert_eq!(
            read_message::<Envelope<Request>>(&mut bytes.as_slice()).unwrap(),
            value
        );
    }

    #[test]
    fn old_notification_config_defaults_recovery_safely() {
        let config: NotificationDeliveryConfig = serde_json::from_str(
            r#"{
                "desktop_enabled": false,
                "sound_enabled": false,
                "blocked": true,
                "completed": true,
                "blocked_sound": "message-new-instant",
                "completed_sound": "complete"
            }"#,
        )
        .unwrap();

        assert!(config.resume_agents);
        assert!(!config.persist_terminal_history);
        assert_eq!(config.max_scheduled_execution_concurrency, 4);
        assert!(!config.scheduled_dispatch_failed);
        assert!(!config.scheduled_interrupted);
    }

    #[test]
    fn typed_errors_are_additive_within_protocol_six() {
        let legacy: Response =
            serde_json::from_str(r#"{"response":"error","message":"legacy daemon"}"#).unwrap();
        assert_eq!(
            legacy,
            Response::Error {
                message: "legacy daemon".into(),
                code: None,
            }
        );

        let coded = Response::Error {
            message: "missing".into(),
            code: Some(ErrorCode::NotFound),
        };
        let LegacyResponse::Error { message } =
            serde_json::from_value(serde_json::to_value(coded).unwrap()).unwrap();
        assert_eq!(message, "missing");

        let unknown: Response =
            serde_json::from_str(r#"{"response":"error","message":"future","code":"future_code"}"#)
                .unwrap();
        assert!(matches!(
            unknown,
            Response::Error {
                code: Some(ErrorCode::Unknown),
                ..
            }
        ));
    }

    #[test]
    fn shell_spec_requires_cwd_on_the_wire() {
        let request = r#"{"request":"create_shell","workspace_id":"w1","shell":{"name":"shell","command":[]}}"#;

        assert!(serde_json::from_str::<Request>(request).is_err());
    }

    #[test]
    fn shell_snapshot_defaults_fields_omitted_by_old_daemons() {
        let snapshot = serde_json::from_str::<ShellSnapshot>(
            r#"{"id":"s1","workspace_id":"w1","name":"shell","cwd":"/tmp","status":"running"}"#,
        )
        .unwrap();

        assert!(snapshot.command.is_empty());
        assert!(snapshot.run.is_none());
        assert!(snapshot.foreground_process.is_none());
        assert!(
            serde_json::to_value(snapshot)
                .unwrap()
                .get("foreground_process")
                .is_none()
        );
    }

    #[test]
    fn workspace_snapshot_defaults_fields_omitted_by_old_daemons() {
        let snapshot = serde_json::from_str::<WorkspaceSnapshot>(
            r#"{"id":"w1","name":"workspace","shells":[]}"#,
        )
        .unwrap();

        assert!(snapshot.launchers.is_empty());
        assert!(snapshot.agents.is_empty());
        assert!(snapshot.schedules.is_empty());
        assert!(snapshot.default_cwd.is_none());
        assert!(
            serde_json::to_value(snapshot)
                .unwrap()
                .get("schedules")
                .is_none()
        );
    }

    #[test]
    fn old_workspace_creation_defaults_to_no_cwd() {
        let request = serde_json::from_str::<Request>(
            r#"{"request":"create_workspace","name":"workspace","shells":[]}"#,
        )
        .unwrap();

        assert!(matches!(
            request,
            Request::CreateWorkspace {
                default_cwd: None,
                ..
            }
        ));
    }

    #[test]
    fn agent_messages_round_trip_with_snake_case_names() {
        let report = AgentReport {
            state: AgentState::Working,
            authority: AgentAuthority::LifecycleIntegration,
            evidence: "tool call in progress".into(),
            confidence: 95,
        };
        let request = Request::RegisterAgent {
            shell_id: "s1".into(),
            run_id: "r1".into(),
            spec: AgentRegistrationSpec {
                name: "opencode".into(),
                integration: "opencode-plugin".into(),
                external_session_id: Some("external-1".into()),
                report: report.clone(),
            },
        };

        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(encoded["request"], "register_agent");
        assert_eq!(encoded["spec"]["report"]["state"], "working");
        assert_eq!(
            encoded["spec"]["report"]["authority"],
            "lifecycle_integration"
        );
        assert_eq!(serde_json::from_value::<Request>(encoded).unwrap(), request);

        let agent = AgentInstanceSnapshot {
            id: "a1".into(),
            workspace_id: "w1".into(),
            shell_id: "s1".into(),
            run_id: "r1".into(),
            name: "opencode".into(),
            integration: "opencode-plugin".into(),
            external_session_id: Some("external-1".into()),
            cwd: Some("/tmp/project".into()),
            started_at_ms: 10,
            ended_at_ms: None,
            observation: AgentObservationSnapshot {
                revision: 1,
                state: report.state,
                authority: report.authority,
                evidence: report.evidence,
                confidence: report.confidence,
                observed_at_ms: 11,
            },
            attention: None,
        };
        let event = DaemonEventKind::AgentStateChanged {
            workspace_id: "w1".into(),
            shell_id: "s1".into(),
            agent,
        };
        let encoded = serde_json::to_value(&event).unwrap();
        assert_eq!(encoded["event"], "agent_state_changed");
        assert_eq!(
            serde_json::from_value::<DaemonEventKind>(encoded).unwrap(),
            event
        );
    }

    #[test]
    fn protocol_version_is_forty_six_with_minimum_six() {
        assert_eq!(PROTOCOL_VERSION, 46);
        assert_eq!(MIN_PROTOCOL_VERSION, 6);
    }

    #[test]
    fn collaborative_exact_run_attachment_is_protocol_forty_four() {
        let request = Request::AttachCollaborative {
            shell_id: "shell-1".into(),
            expected_run_id: "run-1".into(),
            profile: test_profile(),
        };
        let encoded = serde_json::to_value(&request).unwrap();

        assert_eq!(encoded["request"], "attach_collaborative");
        assert_eq!(serde_json::from_value::<Request>(encoded).unwrap(), request);
        assert_eq!(
            request.required_feature(),
            Some(ProtocolFeature::CollaborativeExactRunAttachment)
        );
        assert_eq!(request.minimum_protocol_version(), 44);
        assert_eq!(
            ProtocolFeature::CollaborativeExactRunAttachment.capability_names(),
            &["protocol_44", "collaborative_exact_run_attachment"]
        );
    }

    #[test]
    fn node_identity_messages_round_trip() {
        let request = serde_json::to_value(Request::GetNodeIdentity).unwrap();
        assert_eq!(request["request"], "get_node_identity");
        assert_eq!(
            serde_json::from_value::<Request>(request).unwrap(),
            Request::GetNodeIdentity
        );

        let response = Response::NodeIdentity {
            node_id: "550e8400-e29b-41d4-a716-446655440000".into(),
        };
        let encoded = serde_json::to_value(&response).unwrap();
        assert_eq!(encoded["response"], "node_identity");
        assert_eq!(
            serde_json::from_value::<Response>(encoded).unwrap(),
            response
        );

        let request = serde_json::to_value(Request::OpenFederationChannel).unwrap();
        assert_eq!(request["request"], "open_federation_channel");
        let response = Response::FederationChannel {
            node_id: "550e8400-e29b-41d4-a716-446655440000".into(),
        };
        let encoded = serde_json::to_value(&response).unwrap();
        assert_eq!(encoded["response"], "federation_channel");
        assert_eq!(
            serde_json::from_value::<Response>(encoded).unwrap(),
            response
        );

        let request = Request::RekeyNode {
            expected_node_id: "550e8400-e29b-41d4-a716-446655440000".into(),
        };
        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(encoded["request"], "rekey_node");
        assert_eq!(serde_json::from_value::<Request>(encoded).unwrap(), request);
    }

    #[test]
    fn node_registration_messages_round_trip() {
        let registration = NodeRegistrationSnapshot {
            alias: "work".into(),
            target: "user@work.example".into(),
            node_id: "550e8400-e29b-41d4-a716-446655440000".into(),
            revision: 3,
            tombstone_epoch: 1,
        };
        for request in [
            Request::AddNodeRegistration {
                alias: registration.alias.clone(),
                target: registration.target.clone(),
                node_id: registration.node_id.clone(),
            },
            Request::ListNodeRegistrations,
            Request::GetNodeRegistration {
                selector: registration.alias.clone(),
            },
            Request::RenameNodeRegistration {
                selector: registration.alias.clone(),
                alias: "office".into(),
                expected_revision: 3,
            },
            Request::RetargetNodeRegistration {
                selector: registration.alias.clone(),
                target: "new.example".into(),
                node_id: registration.node_id.clone(),
                expected_revision: 3,
            },
            Request::ForgetNodeRegistration {
                selector: registration.alias.clone(),
            },
        ] {
            let encoded = serde_json::to_value(&request).unwrap();
            assert_eq!(serde_json::from_value::<Request>(encoded).unwrap(), request);
            assert_eq!(
                request.required_feature(),
                Some(ProtocolFeature::NodeRegistration)
            );
        }
        for response in [
            Response::NodeRegistration {
                registration: registration.clone(),
            },
            Response::NodeRegistrations {
                registrations: vec![registration.clone()],
            },
        ] {
            let encoded = serde_json::to_value(&response).unwrap();
            assert_eq!(
                serde_json::from_value::<Response>(encoded).unwrap(),
                response
            );
        }
    }

    #[test]
    fn node_upgrade_maintenance_messages_require_protocol_forty_one() {
        let registration = NodeRegistrationSnapshot {
            alias: "work".into(),
            target: "user@work.example".into(),
            node_id: "550e8400-e29b-41d4-a716-446655440000".into(),
            revision: 3,
            tombstone_epoch: 1,
        };
        for request in [
            Request::BeginNodeUpgradeMaintenance {
                selector: registration.node_id.clone(),
                expected_revision: registration.revision,
            },
            Request::FinishNodeUpgradeMaintenance {
                node_id: registration.node_id.clone(),
                token: "token".into(),
            },
            Request::RenewNodeUpgradeMaintenance {
                node_id: registration.node_id.clone(),
                token: "token".into(),
            },
        ] {
            let encoded = serde_json::to_value(&request).unwrap();
            assert_eq!(serde_json::from_value::<Request>(encoded).unwrap(), request);
            assert_eq!(
                request.required_feature(),
                Some(ProtocolFeature::NodeUpgradeCoordination)
            );
        }
        let response = Response::NodeUpgradeMaintenance {
            registration,
            token: "token".into(),
        };
        let encoded = serde_json::to_value(&response).unwrap();
        assert_eq!(
            serde_json::from_value::<Response>(encoded).unwrap(),
            response
        );
    }

    #[test]
    fn opencode_shared_runtime_claim_messages_require_protocol_forty_two() {
        let generation_id = uuid::Uuid::from_u128(1).to_string();
        let holder_id = uuid::Uuid::from_u128(2).to_string();
        let root_session_id = "ses_root".to_string();
        let shell_id = uuid::Uuid::from_u128(3).to_string();
        let run_id = uuid::Uuid::from_u128(4).to_string();
        let claim_id = uuid::Uuid::from_u128(5).to_string();
        let report = AgentReport {
            state: AgentState::Working,
            authority: AgentAuthority::LifecycleIntegration,
            evidence: "opencode-event".into(),
            confidence: 100,
        };
        for request in [
            Request::EnsureOpenCodeSharedRuntime {
                port: 4096,
                environment: Some(UnixEnvironment {
                    variables: vec![UnixEnvironmentVariable {
                        name: b"OPENCODE_SERVER_PASSWORD".to_vec(),
                        value: b"secret".to_vec(),
                    }],
                }),
            },
            Request::GetOpenCodeSharedRuntime,
            Request::EnsureOpenCodeSessionClaim {
                generation_id: generation_id.clone(),
                holder_id: holder_id.clone(),
                root_session_id: root_session_id.clone(),
                shell_id: shell_id.clone(),
                run_id: run_id.clone(),
                spec: AgentRegistrationSpec {
                    name: "opencode".into(),
                    integration: "opencode-plugin".into(),
                    external_session_id: Some(root_session_id.clone()),
                    report: report.clone(),
                },
            },
            Request::ReleaseOpenCodeSessionClaim {
                generation_id: generation_id.clone(),
                holder_id: holder_id.clone(),
                claim_id: claim_id.clone(),
            },
            Request::ResolveOpenCodeSessionClaim {
                generation_id: generation_id.clone(),
                root_session_id: root_session_id.clone(),
            },
            Request::ReportClaimedOpenCodeAgent {
                generation_id: generation_id.clone(),
                root_session_id: root_session_id.clone(),
                report: report.clone(),
            },
        ] {
            let encoded = serde_json::to_value(&request).unwrap();
            assert!(!format!("{request:?}").contains("secret"));
            assert_eq!(serde_json::from_value::<Request>(encoded).unwrap(), request);
            assert_eq!(
                request.required_feature(),
                Some(ProtocolFeature::OpenCodeSharedRuntimeClaims)
            );
            assert_eq!(request.minimum_protocol_version(), 42);
        }

        let old_request: Request = serde_json::from_value(serde_json::json!({
            "request": "ensure_open_code_shared_runtime",
            "port": 4096
        }))
        .unwrap();
        assert!(matches!(
            old_request,
            Request::EnsureOpenCodeSharedRuntime {
                port: 4096,
                environment: None
            }
        ));

        let claim = OpenCodeSessionClaimSnapshot {
            generation_id: generation_id.clone(),
            claim_id,
            holder_id: holder_id.clone(),
            root_session_id: root_session_id.clone(),
            workspace_id: uuid::Uuid::from_u128(6).to_string(),
            shell_id: shell_id.clone(),
            run_id: run_id.clone(),
            agent_id: uuid::Uuid::from_u128(7).to_string(),
            holder_count: 2,
            holder_expires_at_ms: 123,
        };
        let agent = AgentInstanceSnapshot {
            id: claim.agent_id.clone(),
            workspace_id: claim.workspace_id.clone(),
            shell_id,
            run_id,
            name: "opencode".into(),
            integration: "opencode-plugin".into(),
            external_session_id: Some(root_session_id),
            cwd: Some("/tmp/project".into()),
            started_at_ms: 100,
            ended_at_ms: None,
            observation: AgentObservationSnapshot {
                revision: 1,
                state: report.state,
                authority: report.authority,
                evidence: report.evidence,
                confidence: report.confidence,
                observed_at_ms: 101,
            },
            attention: None,
        };
        for response in [
            Response::OpenCodeSharedRuntime {
                runtime: Some(OpenCodeSharedRuntimeSnapshot {
                    generation_id,
                    url: "http://127.0.0.1:4096".into(),
                    port: 4096,
                    pid: Some(42),
                }),
            },
            Response::OpenCodeSharedRuntime { runtime: None },
            Response::OpenCodeSessionClaim {
                claim,
                agent: agent.clone(),
            },
            Response::OpenCodeSessionClaimReleased { released: true },
            Response::Agent { agent },
        ] {
            let encoded = serde_json::to_value(&response).unwrap();
            assert_eq!(
                serde_json::from_value::<Response>(encoded).unwrap(),
                response
            );
        }
    }

    #[test]
    fn claude_remote_control_binding_messages_require_protocol_forty_three() {
        let binding = ClaudeRemoteControlBindingSnapshot {
            agent_id: "agent-1".into(),
            shell_id: "shell-1".into(),
            run_id: "run-1".into(),
            bridge_session_id: "bridge-1".into(),
        };
        for request in [
            Request::SetClaudeRemoteControlBinding {
                agent_id: binding.agent_id.clone(),
                shell_id: binding.shell_id.clone(),
                run_id: binding.run_id.clone(),
                bridge_session_id: Some(binding.bridge_session_id.clone()),
            },
            Request::SetClaudeRemoteControlBinding {
                agent_id: binding.agent_id.clone(),
                shell_id: binding.shell_id.clone(),
                run_id: binding.run_id.clone(),
                bridge_session_id: None,
            },
            Request::GetClaudeRemoteControlBinding {
                agent_id: binding.agent_id.clone(),
                shell_id: binding.shell_id.clone(),
                run_id: binding.run_id.clone(),
            },
        ] {
            let encoded = serde_json::to_value(&request).unwrap();
            assert!(matches!(
                encoded["request"].as_str(),
                Some("set_claude_remote_control_binding" | "get_claude_remote_control_binding")
            ));
            if matches!(
                &request,
                Request::SetClaudeRemoteControlBinding {
                    bridge_session_id: None,
                    ..
                }
            ) {
                assert!(encoded.get("bridge_session_id").is_none());
            }
            assert_eq!(serde_json::from_value::<Request>(encoded).unwrap(), request);
            assert_eq!(
                request.required_feature(),
                Some(ProtocolFeature::ClaudeRemoteControlBindings)
            );
            assert_eq!(request.minimum_protocol_version(), 43);
        }

        for response in [
            Response::ClaudeRemoteControlBinding {
                binding: Some(binding),
            },
            Response::ClaudeRemoteControlBinding { binding: None },
        ] {
            let encoded = serde_json::to_value(&response).unwrap();
            assert_eq!(encoded["response"], "claude_remote_control_binding");
            assert_eq!(
                serde_json::from_value::<Response>(encoded).unwrap(),
                response
            );
        }
        assert_eq!(
            ProtocolFeature::ClaudeRemoteControlBindings.capability_names(),
            &["protocol_43", "claude_remote_control_bindings"]
        );
        assert_eq!(MAX_CLAUDE_REMOTE_CONTROL_BINDINGS, 4_096);
        assert_eq!(MAX_CLAUDE_BRIDGE_SESSION_ID_BYTES, 256);
    }

    #[test]
    fn kiro_launch_holder_messages_require_protocol_forty_five() {
        let report = AgentReport {
            state: AgentState::Working,
            authority: AgentAuthority::LifecycleIntegration,
            evidence: "Kiro processing prompt".into(),
            confidence: 100,
        };
        for request in [
            Request::AcquireKiroLaunchHolder {
                pid: 42,
                shell_id: "shell-1".into(),
                run_id: "run-1".into(),
            },
            Request::ReportKiroHook {
                holder_id: "holder-1".into(),
                session_id: "session-1".into(),
                report,
            },
            Request::ReleaseKiroLaunchHolder {
                holder_id: "holder-1".into(),
            },
        ] {
            let encoded = serde_json::to_value(&request).unwrap();
            assert_eq!(serde_json::from_value::<Request>(encoded).unwrap(), request);
            assert_eq!(
                request.required_feature(),
                Some(ProtocolFeature::KiroLaunchHolders)
            );
            assert_eq!(request.minimum_protocol_version(), 45);
        }
        for response in [
            Response::KiroLaunchHolder {
                holder_id: "holder-1".into(),
            },
            Response::KiroLaunchHolderReleased { released: true },
        ] {
            let encoded = serde_json::to_value(&response).unwrap();
            assert_eq!(
                serde_json::from_value::<Response>(encoded).unwrap(),
                response
            );
        }
        assert_eq!(MAX_KIRO_LAUNCH_HOLDERS, 256);
        assert_eq!(MAX_KIRO_HOLDER_SESSIONS, 16);
        assert_eq!(MAX_KIRO_SESSION_ID_BYTES, 256);
    }

    #[test]
    fn kiro_stop_idle_requires_protocol_forty_six() {
        let request = Request::ReportKiroHook {
            holder_id: "holder-1".into(),
            session_id: "session-1".into(),
            report: AgentReport {
                state: AgentState::Idle,
                authority: AgentAuthority::LifecycleIntegration,
                evidence: "Kiro session idle".into(),
                confidence: 100,
            },
        };
        assert_eq!(
            request.required_feature(),
            Some(ProtocolFeature::KiroStopIdle)
        );
        assert_eq!(request.minimum_protocol_version(), 46);
        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(serde_json::from_value::<Request>(encoded).unwrap(), request);
    }

    #[test]
    fn node_projection_sync_is_protocol_thirty_two_and_privacy_allowlisted() {
        let request = Request::SyncNodeProjection {
            after: Some(EventCursor {
                stream_id: "stream".into(),
                event_id: 9,
            }),
            wait_ms: 1_000,
        };
        assert_eq!(
            request.required_feature(),
            Some(ProtocolFeature::NodeProjectionSync)
        );
        assert_eq!(
            serde_json::from_value::<Request>(serde_json::to_value(&request).unwrap()).unwrap(),
            request
        );

        let projection = NodeProjectionSnapshot {
            node_id: uuid::Uuid::from_u128(2).to_string(),
            workspaces: Vec::new(),
            shells: Vec::new(),
            launchers: Vec::new(),
            agents: Vec::new(),
            schedules: Vec::new(),
            executions: Vec::new(),
            executions_truncated: false,
            scheduler: SchedulerHealth {
                state: SchedulerState::Active,
                max_concurrent: 4,
                active_executions: 0,
            },
        };
        let mut encoded = serde_json::to_value(projection).unwrap();
        let serialized = encoded.to_string();
        for private in [
            "cwd",
            "command",
            "terminal",
            "prompt",
            "evidence",
            "environment",
            "external_session_id",
            "runner_token",
            "bridge_session_id",
        ] {
            assert!(!serialized.contains(private));
        }
        encoded
            .as_object_mut()
            .unwrap()
            .insert("prompt".into(), serde_json::json!("private"));
        assert!(serde_json::from_value::<NodeProjectionSnapshot>(encoded).is_err());
    }

    #[test]
    fn combined_node_snapshot_is_protocol_thirty_three_and_round_trips() {
        let request = Request::GetCombinedNodeSnapshot {
            selector: Some("work".into()),
        };
        assert_eq!(
            request.required_feature(),
            Some(ProtocolFeature::CombinedNodeSnapshot)
        );
        assert_eq!(
            serde_json::from_value::<Request>(serde_json::to_value(&request).unwrap()).unwrap(),
            request
        );
        let response = Response::CombinedNodeSnapshot {
            snapshot: CombinedNodeSnapshot {
                nodes: Vec::new(),
                workspaces: Vec::new(),
                external_workspaces: Vec::new(),
                focused_terminal: Some(QualifiedFocusedTerminalSnapshot {
                    revision: 4,
                    shell: QualifiedIdentity::new("node", "shell"),
                }),
            },
        };
        assert_eq!(
            serde_json::from_value::<Response>(serde_json::to_value(&response).unwrap()).unwrap(),
            response
        );
        let identity = QualifiedIdentity::new("node", "resource");
        assert_eq!(
            serde_json::to_value(identity).unwrap(),
            serde_json::json!({"node_id": "node", "inner_id": "resource"})
        );
    }

    #[test]
    fn cached_projection_dismissal_is_protocol_forty_and_round_trips() {
        for request in [
            Request::DismissNodeProjectionShell {
                node_id: "node-1".into(),
                shell_id: "shell-1".into(),
            },
            Request::RestoreDismissedNodeProjectionShells {
                node_id: "node-1".into(),
            },
        ] {
            assert_eq!(
                request.required_feature(),
                Some(ProtocolFeature::CachedProjectionDismissal)
            );
            assert_eq!(
                request
                    .required_feature()
                    .map(ProtocolFeature::minimum_version),
                Some(40)
            );
            assert_eq!(
                serde_json::from_value::<Request>(serde_json::to_value(&request).unwrap()).unwrap(),
                request
            );
        }
    }

    #[test]
    fn observed_helper_version_round_trips_and_defaults() {
        let health: NodeProjectionHealth = serde_json::from_value(serde_json::json!({
            "code": "online",
            "stale": false,
            "cache_generation": 1,
            "capabilities": []
        }))
        .unwrap();
        assert_eq!(health.observed_helper_version, None);

        let mut encoded = serde_json::to_value(&health).unwrap();
        assert!(encoded.get("observed_helper_version").is_none());
        encoded["observed_helper_version"] = serde_json::json!("0.41.0");
        let observed: NodeProjectionHealth = serde_json::from_value(encoded).unwrap();
        assert_eq!(observed.observed_helper_version.as_deref(), Some("0.41.0"));
        assert_eq!(
            serde_json::from_value::<NodeProjectionHealth>(
                serde_json::to_value(&observed).unwrap()
            )
            .unwrap(),
            observed
        );

        let node: CombinedNode = serde_json::from_value(serde_json::json!({
            "node_id": "node",
            "alias": "remote",
            "local": false,
            "health": "online",
            "current": true,
            "stale": false,
            "observed_at_ms": 1,
            "observed_capabilities": [],
            "observed_helper_version": "0.41.0",
            "scheduler": {
                "state": "active",
                "max_concurrent": 4,
                "active_executions": 0
            }
        }))
        .unwrap();
        assert_eq!(node.observed_helper_version.as_deref(), Some("0.41.0"));
        assert_eq!(
            serde_json::from_value::<CombinedNode>(serde_json::to_value(&node).unwrap()).unwrap(),
            node
        );
    }

    #[test]
    fn protocol_thirty_four_routes_only_closed_typed_operations() {
        let operation = RoutedOperation::RestartShell {
            shell_id: "shell".into(),
            expected_revision: 7,
            expected_run_id: "run".into(),
        };
        let request = Request::RouteNodeOperation {
            node_id: uuid::Uuid::from_u128(2).to_string(),
            operation: operation.clone(),
        };
        assert_eq!(
            request.required_feature(),
            Some(ProtocolFeature::GuardedNodeRouting)
        );
        assert_eq!(
            serde_json::from_value::<Request>(serde_json::to_value(&request).unwrap()).unwrap(),
            request
        );
        assert_eq!(
            operation.owner_request(),
            Request::GuardedRestartShell {
                shell_id: "shell".into(),
                expected_revision: 7,
                expected_run_id: "run".into(),
            }
        );
        assert!(!operation.is_retryable());
        assert!(
            serde_json::from_value::<RoutedOperation>(serde_json::json!({
                "operation": "shutdown"
            }))
            .is_err()
        );
    }

    #[test]
    fn protocol_thirty_seven_routes_remote_schedule_reads_without_broadening_retries() {
        let create = RoutedOperation::CreateAgentSchedule {
            workspace_id: "workspace-1".into(),
            spec: AgentScheduleSpec {
                name: "review".into(),
                cwd: "/owner/work".into(),
                integration: "opencode".into(),
                prompt: "PRIVATE ROUTED PROMPT".into(),
                session: AgentScheduleSession::Fresh,
                trigger: AgentScheduleTrigger {
                    cron: "0 2 * * *".into(),
                    timezone: "UTC".into(),
                },
                state: AgentScheduleState::Paused,
                overlap_policy: AgentScheduleOverlapPolicy::Skip,
            },
        };
        let request = Request::RouteNodeOperation {
            node_id: "node-1".into(),
            operation: create.clone(),
        };
        assert_eq!(
            request.required_feature(),
            Some(ProtocolFeature::RemoteSchedules)
        );
        assert!(!create.is_retryable());
        assert!(format!("{create:?}").contains("<redacted>"));
        assert!(!format!("{create:?}").contains("PRIVATE ROUTED PROMPT"));

        let list = RoutedOperation::ListScheduledExecutions {
            workspace_id: None,
            schedule_id: Some("schedule-1".into()),
            limit: 100,
        };
        assert!(list.is_retryable());
        assert_eq!(
            serde_json::from_value::<RoutedOperation>(serde_json::to_value(&list).unwrap())
                .unwrap(),
            list
        );
        assert!(
            RoutedOperation::RunAgentSchedule {
                schedule_id: "schedule-1".into(),
                dispatch_key: "dispatch-1".into(),
            }
            .is_retryable()
        );
        assert!(
            !RoutedOperation::CancelScheduledExecution {
                execution_id: "execution-1".into(),
                expected_revision: 4,
            }
            .is_retryable()
        );
    }

    #[test]
    fn protocol_thirty_eight_coordinates_workspaces_and_exact_owner_creation() {
        let node_id = uuid::Uuid::from_u128(1).to_string();
        let workspace_id = uuid::Uuid::from_u128(2).to_string();
        let requests = vec![
            Request::CreateGlobalWorkspace {
                name: "work".into(),
            },
            Request::AdoptNodeWorkspace {
                identity: QualifiedIdentity::new(&node_id, &workspace_id),
                expected_revision: 3,
            },
            Request::LinkNodeWorkspace {
                global_workspace_id: uuid::Uuid::from_u128(3).to_string(),
                expected_global_revision: 4,
                identity: QualifiedIdentity::new(&node_id, &workspace_id),
                expected_owner_revision: 3,
            },
            Request::OpenGlobalWorkspace {
                workspace_id: workspace_id.clone(),
                expected_revision: 4,
            },
            Request::CloseGlobalWorkspace {
                workspace_id: workspace_id.clone(),
                expected_revision: 4,
            },
            Request::RetryGlobalWorkspaceClose {
                workspace_id: workspace_id.clone(),
            },
            Request::CreateGlobalWorkspaceWithShell {
                operation_id: uuid::Uuid::from_u128(11).to_string(),
                global_workspace_id: uuid::Uuid::from_u128(12).to_string(),
                name: "project".into(),
                node_id: node_id.clone(),
                owner_workspace_id: uuid::Uuid::from_u128(13).to_string(),
                default_cwd: "/owner/project".into(),
                shell_id: uuid::Uuid::from_u128(14).to_string(),
                shell: ShellSpec {
                    name: "project".into(),
                    cwd: "/owner/project".into(),
                    command: Vec::new(),
                },
            },
        ];
        for request in requests {
            assert_eq!(
                request.required_feature(),
                Some(ProtocolFeature::GlobalWorkspaces)
            );
            assert_eq!(
                serde_json::from_value::<Request>(serde_json::to_value(&request).unwrap()).unwrap(),
                request
            );
        }

        let operation = RoutedOperation::CreateWorkspaceShell {
            workspace_id: workspace_id.clone(),
            workspace_name: "work".into(),
            default_cwd: Some("/owner/work".into()),
            shell_id: uuid::Uuid::from_u128(4).to_string(),
            shell: ShellSpec {
                name: "shell".into(),
                cwd: "/owner/work".into(),
                command: vec!["bash".into(), "-lc".into(), "printf %s safe".into()],
            },
        };
        assert!(operation.is_retryable());
        assert_eq!(operation.classification().guard, RoutedGuard::ExactId);
        assert!(matches!(
            operation.owner_request(),
            Request::CreateWorkspaceShell { .. }
        ));
        let request = Request::RouteNodeOperation { node_id, operation };
        assert_eq!(
            request.required_feature(),
            Some(ProtocolFeature::GlobalWorkspaces)
        );

        let schedule = RoutedOperation::CreateWorkspaceAgentSchedule {
            workspace_id,
            workspace_name: "work".into(),
            default_cwd: Some("/owner/work".into()),
            schedule_id: uuid::Uuid::from_u128(5).to_string(),
            spec: AgentScheduleSpec {
                name: "review".into(),
                cwd: "/owner/work".into(),
                integration: "opencode".into(),
                prompt: "PRIVATE WORKSPACE PROMPT".into(),
                session: AgentScheduleSession::Fresh,
                trigger: AgentScheduleTrigger {
                    cron: "0 2 * * *".into(),
                    timezone: "UTC".into(),
                },
                state: AgentScheduleState::Paused,
                overlap_policy: AgentScheduleOverlapPolicy::Skip,
            },
        };
        assert!(!format!("{schedule:?}").contains("PRIVATE WORKSPACE PROMPT"));
        assert!(format!("{schedule:?}").contains("<redacted>"));

        let request = Request::CreateGlobalWorkspaceAgentSchedule {
            operation_id: uuid::Uuid::from_u128(10).to_string(),
            global_workspace_id: uuid::Uuid::from_u128(6).to_string(),
            expected_global_revision: 1,
            node_id: uuid::Uuid::from_u128(7).to_string(),
            owner_workspace_id: uuid::Uuid::from_u128(8).to_string(),
            default_cwd: Some("/owner/work".into()),
            schedule_id: uuid::Uuid::from_u128(9).to_string(),
            spec: match schedule {
                RoutedOperation::CreateWorkspaceAgentSchedule { spec, .. } => spec,
                _ => unreachable!(),
            },
        };
        assert_eq!(
            request.required_feature(),
            Some(ProtocolFeature::GlobalWorkspaces)
        );
        assert!(!format!("{request:?}").contains("PRIVATE WORKSPACE PROMPT"));
        assert_eq!(
            serde_json::from_value::<Request>(serde_json::to_value(&request).unwrap()).unwrap(),
            request
        );
    }

    #[test]
    fn agent_schedule_defaults_and_snake_case_are_stable() {
        assert_eq!(AgentScheduleSession::default(), AgentScheduleSession::Fresh);
        assert_eq!(AgentScheduleState::default(), AgentScheduleState::Paused);
        assert_eq!(
            AgentScheduleOverlapPolicy::default(),
            AgentScheduleOverlapPolicy::Skip
        );

        let spec: AgentScheduleSpec = serde_json::from_value(serde_json::json!({
            "name": "review",
            "cwd": "/tmp/project",
            "integration": "opencode",
            "prompt": "review the changes",
            "trigger": {"cron": "0 9 * * 1-5", "timezone": "UTC"}
        }))
        .unwrap();
        assert_eq!(spec.session, AgentScheduleSession::Fresh);
        assert_eq!(spec.state, AgentScheduleState::Paused);
        assert_eq!(spec.overlap_policy, AgentScheduleOverlapPolicy::Skip);

        let continued = serde_json::to_value(AgentScheduleSession::Continue {
            external_session_id: "session-1".into(),
        })
        .unwrap();
        assert_eq!(
            continued,
            serde_json::json!({"continue": {"external_session_id": "session-1"}})
        );
        assert_eq!(
            serde_json::to_value(AgentScheduleState::Enabled).unwrap(),
            "enabled"
        );
        assert_eq!(
            serde_json::to_value(AgentScheduleOverlapPolicy::Skip).unwrap(),
            "skip"
        );
    }

    #[test]
    fn agent_schedule_messages_round_trip() {
        let spec = AgentScheduleSpec {
            name: "review".into(),
            cwd: "/tmp/project".into(),
            integration: "opencode".into(),
            prompt: "review the changes".into(),
            session: AgentScheduleSession::Fresh,
            trigger: AgentScheduleTrigger {
                cron: "0 9 * * 1-5".into(),
                timezone: "UTC".into(),
            },
            state: AgentScheduleState::Paused,
            overlap_policy: AgentScheduleOverlapPolicy::Skip,
        };
        let update = AgentScheduleUpdate {
            name: "updated review".into(),
            prompt: "private updated prompt".into(),
            trigger: AgentScheduleTrigger {
                cron: "30 10 * * 1-5".into(),
                timezone: "America/New_York".into(),
            },
        };
        let requests = [
            Request::CreateAgentSchedule {
                workspace_id: "w1".into(),
                spec,
            },
            Request::GetAgentSchedule {
                schedule_id: "schedule-1".into(),
            },
            Request::UpdateAgentSchedule {
                schedule_id: "schedule-1".into(),
                expected_revision: 3,
                update: update.clone(),
            },
            Request::PauseAgentSchedule {
                schedule_id: "schedule-1".into(),
            },
            Request::ResumeAgentSchedule {
                schedule_id: "schedule-1".into(),
            },
            Request::RemoveAgentSchedule {
                schedule_id: "schedule-1".into(),
            },
        ];
        for request in requests {
            let encoded = serde_json::to_value(&request).unwrap();
            assert_eq!(serde_json::from_value::<Request>(encoded).unwrap(), request);
        }
        assert_eq!(
            Request::UpdateAgentSchedule {
                schedule_id: "schedule-1".into(),
                expected_revision: 3,
                update: update.clone(),
            }
            .required_feature(),
            Some(ProtocolFeature::AgentScheduleEditing)
        );
        assert!(!format!("{update:?}").contains("private updated prompt"));

        let schedule = test_schedule();
        for response in [
            Response::AgentSchedule {
                schedule: schedule.clone(),
            },
            Response::AgentScheduleInspection {
                inspection: AgentScheduleInspection {
                    schedule,
                    prompt: "private prompt".into(),
                },
            },
        ] {
            let encoded = serde_json::to_value(&response).unwrap();
            assert_eq!(
                serde_json::from_value::<Response>(encoded).unwrap(),
                response
            );
        }
    }

    #[test]
    fn scheduled_execution_wire_and_events_are_prompt_free() {
        let execution = ScheduledExecutionSnapshot {
            id: "execution-1".into(),
            workspace_id: "workspace-1".into(),
            schedule_id: "schedule-1".into(),
            revision: 2,
            state: ScheduledExecutionState::Starting,
            dispatch_kind: ScheduledExecutionDispatchKind::Manual,
            dispatch_key: "dispatch-1".into(),
            schedule_revision: 3,
            prompt_revision: 2,
            trigger_revision: 1,
            requested_at_ms: 10,
            scheduled_at_ms: None,
            coalesced_through_ms: None,
            started_at_ms: None,
            ended_at_ms: None,
            cwd: "/tmp/project".into(),
            integration: "opencode".into(),
            session: AgentScheduleSession::Fresh,
            reason: None,
            outcome: None,
            shell_id: Some("shell-1".into()),
            run_id: Some("run-1".into()),
            agent_id: None,
            external_session_id: None,
        };
        let prompt = "PRIVATE EXECUTION PROMPT";
        let claim = ScheduledExecutionClaim {
            execution: execution.clone(),
            prompt: prompt.into(),
        };
        assert!(!format!("{claim:?}").contains(prompt));
        let event = DaemonEventKind::ScheduledExecutionChanged {
            workspace_id: execution.workspace_id.clone(),
            execution: execution.clone(),
        };
        assert!(!serde_json::to_string(&event).unwrap().contains(prompt));
        let request = Request::RunAgentSchedule {
            schedule_id: execution.schedule_id.clone(),
            dispatch_key: execution.dispatch_key.clone(),
        };
        assert_eq!(
            serde_json::from_value::<Request>(serde_json::to_value(&request).unwrap()).unwrap(),
            request
        );
        assert_eq!(
            request.required_feature(),
            Some(ProtocolFeature::ScheduledExecutions)
        );
        let capability = "PRIVATE RUNNER CAPABILITY";
        let request = Request::ResolveScheduledExecutionClaim {
            schedule_id: execution.schedule_id,
            shell_id: execution.shell_id.unwrap(),
            run_id: execution.run_id.unwrap(),
            runner_token: ScheduledRunnerCapability::new(capability),
        };
        assert!(!format!("{request:?}").contains(capability));
        assert_eq!(
            serde_json::from_value::<Request>(serde_json::to_value(&request).unwrap()).unwrap(),
            request
        );
    }

    #[test]
    fn schedule_summaries_and_events_never_expose_prompts() {
        let private_prompt = "private prompt contents";
        let schedule = test_schedule();
        let request = Request::CreateAgentSchedule {
            workspace_id: "w1".into(),
            spec: AgentScheduleSpec {
                name: "review".into(),
                cwd: "/tmp/project".into(),
                integration: "opencode".into(),
                prompt: private_prompt.into(),
                session: AgentScheduleSession::Fresh,
                trigger: schedule.trigger.clone(),
                state: AgentScheduleState::Paused,
                overlap_policy: AgentScheduleOverlapPolicy::Skip,
            },
        };
        let request_debug = format!("{request:?}");
        assert!(!request_debug.contains(private_prompt));
        assert!(request_debug.contains("redacted"));

        let summary = serde_json::to_value(&schedule).unwrap();
        assert!(summary.get("prompt").is_none());
        assert!(!summary.to_string().contains(private_prompt));

        let events = [
            DaemonEventKind::AgentScheduleCreated {
                workspace_id: "w1".into(),
                schedule: schedule.clone(),
            },
            DaemonEventKind::AgentSchedulePaused {
                workspace_id: "w1".into(),
                schedule: schedule.clone(),
            },
            DaemonEventKind::AgentScheduleResumed {
                workspace_id: "w1".into(),
                schedule: schedule.clone(),
            },
            DaemonEventKind::AgentScheduleUpdated {
                workspace_id: "w1".into(),
                schedule: schedule.clone(),
            },
            DaemonEventKind::AgentScheduleRemoved {
                workspace_id: "w1".into(),
                schedule_id: schedule.id.clone(),
            },
        ];
        for event in events {
            let encoded = serde_json::to_value(&event).unwrap();
            assert!(!encoded.to_string().contains(private_prompt));
            assert_eq!(
                serde_json::from_value::<DaemonEventKind>(encoded).unwrap(),
                event
            );
        }

        let inspection = AgentScheduleInspection {
            schedule,
            prompt: private_prompt.into(),
        };
        let debug = format!("{inspection:?}");
        assert!(!debug.contains(private_prompt));
        assert!(debug.contains("redacted"));
        assert_eq!(
            serde_json::to_value(inspection).unwrap()["prompt"],
            private_prompt
        );
    }

    #[test]
    fn terminal_preview_styles_round_trip() {
        let preview = TerminalPreview {
            lines: vec![TerminalPreviewLine {
                spans: vec![TerminalPreviewSpan {
                    text: "styled".into(),
                    style: TerminalStyle {
                        foreground: TerminalColor::Indexed(2),
                        background: TerminalColor::Rgb {
                            red: 3,
                            green: 4,
                            blue: 5,
                        },
                        bold: true,
                        dim: false,
                        italic: true,
                        underline: true,
                        inverse: true,
                    },
                }],
            }],
        };
        let response = Response::ShellPreview {
            preview: preview.clone(),
        };
        let encoded = serde_json::to_value(response).unwrap();

        assert_eq!(encoded["response"], "shell_preview");
        assert_eq!(
            serde_json::from_value::<Response>(encoded).unwrap(),
            Response::ShellPreview { preview }
        );
    }

    #[test]
    fn old_snapshot_defaults_focused_terminal() {
        let snapshot: Snapshot = serde_json::from_str(r#"{"workspaces":[]}"#).unwrap();

        assert_eq!(snapshot.focused_terminal, None);
    }

    #[test]
    fn focused_terminal_read_round_trips() {
        let request = serde_json::to_value(Request::GetFocusedTerminal).unwrap();
        assert_eq!(request["request"], "get_focused_terminal");
        assert_eq!(
            serde_json::from_value::<Request>(request).unwrap(),
            Request::GetFocusedTerminal
        );
        let response = Response::FocusedTerminal {
            focused_terminal: Some(FocusedTerminalSnapshot {
                revision: 1,
                workspace_id: "w1".into(),
                shell_id: "s1".into(),
                run_id: "r1".into(),
            }),
        };
        let encoded = serde_json::to_value(&response).unwrap();
        assert_eq!(encoded["response"], "focused_terminal");
        assert_eq!(
            serde_json::from_value::<Response>(encoded).unwrap(),
            response
        );
    }

    #[test]
    fn request_feature_requirements_cover_all_groups() {
        let groups = vec![
            (
                33,
                vec![Request::GetCombinedNodeSnapshot { selector: None }],
            ),
            (
                31,
                vec![
                    Request::AddNodeRegistration {
                        alias: "work".into(),
                        target: "work.example".into(),
                        node_id: "550e8400-e29b-41d4-a716-446655440000".into(),
                    },
                    Request::ListNodeRegistrations,
                    Request::GetNodeRegistration {
                        selector: "work".into(),
                    },
                    Request::RenameNodeRegistration {
                        selector: "work".into(),
                        alias: "office".into(),
                        expected_revision: 1,
                    },
                    Request::RetargetNodeRegistration {
                        selector: "work".into(),
                        target: "new.example".into(),
                        node_id: "550e8400-e29b-41d4-a716-446655440000".into(),
                        expected_revision: 1,
                    },
                    Request::ForgetNodeRegistration {
                        selector: "work".into(),
                    },
                ],
            ),
            (
                30,
                vec![Request::RekeyNode {
                    expected_node_id: "550e8400-e29b-41d4-a716-446655440000".into(),
                }],
            ),
            (29, vec![Request::OpenFederationChannel]),
            (28, vec![Request::GetNodeIdentity]),
            (
                25,
                vec![Request::WaitScheduledExecution {
                    execution_id: "execution-1".into(),
                    after_revision: 1,
                    wait_ms: 30_000,
                }],
            ),
            (
                22,
                vec![
                    Request::CreateAgentSchedule {
                        workspace_id: "w1".into(),
                        spec: AgentScheduleSpec {
                            name: "review".into(),
                            cwd: "/tmp".into(),
                            integration: "opencode".into(),
                            prompt: "review".into(),
                            session: AgentScheduleSession::Fresh,
                            trigger: AgentScheduleTrigger {
                                cron: "0 9 * * *".into(),
                                timezone: "UTC".into(),
                            },
                            state: AgentScheduleState::Paused,
                            overlap_policy: AgentScheduleOverlapPolicy::Skip,
                        },
                    },
                    Request::GetAgentSchedule {
                        schedule_id: "schedule-1".into(),
                    },
                    Request::PauseAgentSchedule {
                        schedule_id: "schedule-1".into(),
                    },
                    Request::ResumeAgentSchedule {
                        schedule_id: "schedule-1".into(),
                    },
                    Request::RemoveAgentSchedule {
                        schedule_id: "schedule-1".into(),
                    },
                ],
            ),
            (21, vec![Request::GetFocusedTerminal]),
            (
                20,
                vec![Request::ReadShellPreview {
                    shell_id: "s1".into(),
                    max_bytes: 1024,
                    max_lines: 500,
                }],
            ),
            (
                19,
                vec![
                    Request::CreateWorkspace {
                        name: "project".into(),
                        default_cwd: Some("/tmp/project".into()),
                        shells: Vec::new(),
                    },
                    Request::CreateShell {
                        workspace_id: None,
                        shell: ShellSpec::login("shell", "/tmp/project"),
                    },
                ],
            ),
            (
                17,
                vec![Request::RestartWithNotificationConfig {
                    notifications: NotificationDeliveryConfig {
                        desktop_enabled: true,
                        sound_enabled: true,
                        blocked: true,
                        completed: true,
                        blocked_sound: "dialog-warning".into(),
                        completed_sound: "complete".into(),
                        resume_agents: true,
                        persist_terminal_history: false,
                        max_scheduled_execution_concurrency: 4,
                        ..Default::default()
                    },
                    environment: None,
                }],
            ),
            (
                6,
                vec![
                    Request::Ping,
                    Request::Restart,
                    Request::Shutdown,
                    Request::Snapshot,
                    Request::GetWorkspace {
                        workspace_id: "w1".into(),
                    },
                    Request::GetShell {
                        shell_id: "s1".into(),
                    },
                    Request::CreateWorkspace {
                        name: "workspace".into(),
                        default_cwd: None,
                        shells: Vec::new(),
                    },
                    Request::CreateShell {
                        workspace_id: Some("w1".into()),
                        shell: ShellSpec::login("shell", "/tmp"),
                    },
                    Request::ReadShell {
                        shell_id: "s1".into(),
                        max_bytes: 1024,
                    },
                    Request::RenameWorkspace {
                        workspace_id: "w1".into(),
                        name: "renamed".into(),
                    },
                    Request::RenameShell {
                        shell_id: "s1".into(),
                        name: "renamed".into(),
                    },
                    Request::CloseWorkspace {
                        workspace_id: "w1".into(),
                    },
                    Request::CloseShell {
                        shell_id: "s1".into(),
                    },
                    Request::Attach {
                        shell_id: "s1".into(),
                        takeover: false,
                        restart_exited: false,
                        expected_run_id: None,
                        profile: test_profile(),
                        environment: None,
                        owner_environment: false,
                    },
                ],
            ),
            (
                7,
                vec![
                    Request::ReadShellAt {
                        shell_id: "s1".into(),
                        max_bytes: 1024,
                        run_id: None,
                        after_revision: None,
                        wait_ms: 0,
                    },
                    Request::Events {
                        after: None,
                        limit: 1,
                        wait_ms: 0,
                    },
                ],
            ),
            (
                8,
                vec![
                    Request::GetLauncher {
                        launcher_id: "l1".into(),
                    },
                    Request::CreateLauncher {
                        workspace_id: "w1".into(),
                        spec: WorkspaceLauncherSpec {
                            name: "editor".into(),
                            command: vec!["editor".into()],
                            cwd: "/tmp".into(),
                        },
                    },
                    Request::RenameLauncher {
                        launcher_id: "l1".into(),
                        name: "renamed".into(),
                    },
                    Request::RemoveLauncher {
                        launcher_id: "l1".into(),
                    },
                ],
            ),
            (
                9,
                vec![
                    Request::GetAgent {
                        agent_id: "a1".into(),
                    },
                    Request::RegisterAgent {
                        shell_id: "s1".into(),
                        run_id: "r1".into(),
                        spec: test_registration(AgentState::Working),
                    },
                    Request::ReportAgent {
                        agent_id: "a1".into(),
                        run_id: "r1".into(),
                        report: test_report(AgentState::Idle),
                    },
                ],
            ),
            (
                10,
                vec![Request::EnsureAgent {
                    shell_id: "s1".into(),
                    run_id: "r1".into(),
                    spec: test_registration(AgentState::Working),
                }],
            ),
            (
                11,
                vec![
                    Request::RestartShell {
                        shell_id: "s1".into(),
                    },
                    Request::Attach {
                        shell_id: "s1".into(),
                        takeover: false,
                        restart_exited: true,
                        expected_run_id: None,
                        profile: test_profile(),
                        environment: None,
                        owner_environment: false,
                    },
                ],
            ),
            (
                12,
                vec![
                    Request::RegisterAgent {
                        shell_id: "s1".into(),
                        run_id: "r1".into(),
                        spec: test_registration(AgentState::Inactive),
                    },
                    Request::EnsureAgent {
                        shell_id: "s1".into(),
                        run_id: "r1".into(),
                        spec: test_registration(AgentState::Inactive),
                    },
                    Request::ReportAgent {
                        agent_id: "a1".into(),
                        run_id: "r1".into(),
                        report: test_report(AgentState::Inactive),
                    },
                ],
            ),
            (
                14,
                vec![Request::WaitAgent {
                    agent_id: "a1".into(),
                    after_revision: 1,
                    wait_ms: 0,
                }],
            ),
            (
                15,
                vec![Request::AcknowledgeAgentAttention {
                    agent_id: "a1".into(),
                    observation_revision: 1,
                }],
            ),
            (
                16,
                vec![Request::Attach {
                    shell_id: "s1".into(),
                    takeover: false,
                    restart_exited: true,
                    expected_run_id: None,
                    profile: test_profile(),
                    environment: Some(UnixEnvironment {
                        variables: Vec::new(),
                    }),
                    owner_environment: false,
                }],
            ),
            (
                26,
                vec![Request::Attach {
                    shell_id: "s1".into(),
                    takeover: true,
                    restart_exited: false,
                    expected_run_id: Some("r1".into()),
                    profile: test_profile(),
                    environment: None,
                    owner_environment: false,
                }],
            ),
            (
                27,
                vec![Request::UpdateAgentSchedule {
                    schedule_id: "schedule-1".into(),
                    expected_revision: 1,
                    update: AgentScheduleUpdate {
                        name: "review".into(),
                        prompt: "private prompt".into(),
                        trigger: AgentScheduleTrigger {
                            cron: "0 9 * * *".into(),
                            timezone: "UTC".into(),
                        },
                    },
                }],
            ),
            (
                35,
                vec![
                    Request::Attach {
                        shell_id: "s1".into(),
                        takeover: true,
                        restart_exited: true,
                        expected_run_id: None,
                        profile: test_profile(),
                        environment: None,
                        owner_environment: true,
                    },
                    Request::AttachNode {
                        identity: QualifiedIdentity::new("node-1", "s1"),
                        takeover: true,
                        restart_exited: true,
                        expected_run_id: None,
                        profile: test_profile(),
                    },
                ],
            ),
            (
                36,
                vec![
                    Request::HostService {
                        operation: HostServiceOperation::DiscoverProjects,
                    },
                    Request::RouteNodeHostService {
                        node_id: "node-1".into(),
                        operation: HostServiceOperation::DiscoverProjects,
                    },
                    Request::ResumeAgentSession {
                        session_id: "session-1".into(),
                        profile: test_profile(),
                    },
                    Request::ResumeNodeAgentSession {
                        node_id: "node-1".into(),
                        session_id: "session-1".into(),
                        profile: test_profile(),
                    },
                ],
            ),
            (
                37,
                vec![Request::RouteNodeOperation {
                    node_id: "node-1".into(),
                    operation: RoutedOperation::WaitScheduledExecution {
                        execution_id: "execution-1".into(),
                        after_revision: 1,
                        wait_ms: 10,
                    },
                }],
            ),
            (
                38,
                vec![
                    Request::CreateGlobalWorkspace {
                        name: "work".into(),
                    },
                    Request::AdoptNodeWorkspace {
                        identity: QualifiedIdentity::new("node-1", "owner-1"),
                        expected_revision: 1,
                    },
                    Request::LinkNodeWorkspace {
                        global_workspace_id: "global-1".into(),
                        expected_global_revision: 1,
                        identity: QualifiedIdentity::new("node-1", "owner-1"),
                        expected_owner_revision: 1,
                    },
                    Request::OpenGlobalWorkspace {
                        workspace_id: "global-1".into(),
                        expected_revision: 1,
                    },
                    Request::CloseGlobalWorkspace {
                        workspace_id: "global-1".into(),
                        expected_revision: 1,
                    },
                    Request::RetryGlobalWorkspaceClose {
                        workspace_id: "global-1".into(),
                    },
                    Request::CreateGlobalWorkspaceShell {
                        operation_id: "operation-1".into(),
                        global_workspace_id: "global-1".into(),
                        expected_global_revision: 1,
                        node_id: "node-1".into(),
                        owner_workspace_id: "owner-1".into(),
                        default_cwd: Some("/owner/work".into()),
                        shell_id: "shell-1".into(),
                        shell: ShellSpec {
                            name: "shell".into(),
                            cwd: "/owner/work".into(),
                            command: Vec::new(),
                        },
                    },
                ],
            ),
        ];

        for (expected, requests) in groups {
            for request in requests {
                assert_eq!(
                    request
                        .required_feature()
                        .map_or(MIN_PROTOCOL_VERSION, ProtocolFeature::minimum_version),
                    expected,
                    "unexpected minimum protocol version for {request:?}"
                );
            }
        }
    }

    #[test]
    fn protocol_features_have_stable_versions_and_capability_names() {
        let expected = [
            (
                6,
                &[
                    "typed_errors",
                    "shell_run_identity",
                    "rendered_scrollback",
                    "graceful_live_handoff",
                    "graceful_exited_handoff",
                ][..],
            ),
            (
                7,
                &[
                    "daemon_events",
                    "reconnectable_event_cursors",
                    "revision_aware_reads",
                ][..],
            ),
            (8, &["workspace_launchers"][..]),
            (
                9,
                &["run_scoped_agent_instances", "agent_authority_precedence"][..],
            ),
            (10, &["protocol_10", "idempotent_agent_ensure"][..]),
            (11, &["protocol_11", "restartable_exited_shells"][..]),
            (
                12,
                &[
                    "protocol_12",
                    "inactive_agent_state",
                    "projected_agent_sessions",
                ][..],
            ),
            (13, &["protocol_13", "durable_session_source_context"][..]),
            (14, &["protocol_14", "revision_aware_agent_wait"][..]),
            (15, &["protocol_15", "persistent_agent_attention"][..]),
            (16, &["protocol_16"][..]),
            (17, &["protocol_17"][..]),
            (18, &["protocol_18", "focused_terminal_following"][..]),
            (19, &["protocol_19", "workspace_default_cwd"][..]),
            (20, &["protocol_20", "structured_terminal_previews"][..]),
            (21, &["protocol_21", "focused_terminal_read"][..]),
            (
                22,
                &[
                    "protocol_22",
                    "agent_schedule_management",
                    "durable_agent_schedules",
                ][..],
            ),
            (
                23,
                &[
                    "protocol_23",
                    "scheduled_execution_dispatch",
                    "scheduled_execution_cancellation",
                    "schedule_owned_shells",
                ][..],
            ),
            (
                24,
                &[
                    "protocol_24",
                    "timed_schedule_dispatch",
                    "scheduler_health",
                    "bounded_scheduled_execution_concurrency",
                ][..],
            ),
            (
                25,
                &[
                    "protocol_25",
                    "revision_aware_scheduled_execution_wait",
                    "bounded_scheduled_execution_history",
                    "scheduled_execution_notifications",
                ][..],
            ),
            (26, &["protocol_26", "exact_run_attachment"][..]),
            (27, &["protocol_27", "agent_schedule_editing"][..]),
            (28, &["protocol_28", "stable_node_identity"][..]),
            (29, &["protocol_29", "federation_daemon_channel"][..]),
            (30, &["protocol_30", "node_rekey"][..]),
            (
                31,
                &[
                    "protocol_31",
                    "node_registration_management",
                    "pinned_node_identity",
                ][..],
            ),
            (
                32,
                &[
                    "protocol_32",
                    "node_projection_sync",
                    "bounded_remote_node_projections",
                ][..],
            ),
            (
                33,
                &[
                    "protocol_33",
                    "combined_node_snapshot",
                    "node_qualified_dashboard",
                ][..],
            ),
            (
                34,
                &[
                    "protocol_34",
                    "typed_exact_node_routing",
                    "guarded_remote_management",
                ][..],
            ),
            (
                35,
                &[
                    "protocol_35",
                    "remote_pty_attachment",
                    "owner_environment_attachment",
                ][..],
            ),
            (
                36,
                &[
                    "protocol_36",
                    "typed_node_host_services",
                    "remote_project_discovery",
                    "remote_launcher_invocation",
                    "remote_integration_management",
                    "remote_agent_session_catalog",
                    "remote_exact_session_resume",
                ][..],
            ),
            (
                37,
                &[
                    "protocol_37",
                    "remote_agent_schedule_management",
                    "remote_scheduled_execution_observation",
                ][..],
            ),
            (
                38,
                &[
                    "protocol_38",
                    "global_workspaces",
                    "multi_node_workspace_placements",
                    "guarded_workspace_adoption",
                    "resumable_workspace_close",
                ][..],
            ),
            (39, &["protocol_39", "qualified_focused_terminal"][..]),
            (40, &["protocol_40", "recovered_agent_presentation"][..]),
            (40, &["cached_projection_dismissal"][..]),
            (41, &["protocol_41", "observed_node_helper_version"][..]),
            (41, &["node_upgrade_coordination"][..]),
            (42, &["protocol_42", "opencode_shared_runtime_claims"][..]),
            (43, &["protocol_43", "claude_remote_control_bindings"][..]),
            (
                44,
                &["protocol_44", "collaborative_exact_run_attachment"][..],
            ),
            (45, &["protocol_45", "kiro_exact_launch_holders"][..]),
            (46, &["protocol_46", "kiro_stop_idle"][..]),
        ];

        let actual = ProtocolFeature::ALL
            .iter()
            .map(|feature| (feature.minimum_version(), feature.capability_names()))
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
        assert!(ProtocolFeature::StructuredTerminalPreview.is_supported_by(PROTOCOL_VERSION));
        assert!(ProtocolFeature::AgentSchedules.is_supported_by(PROTOCOL_VERSION));
        assert_eq!(
            protocol_capabilities().collect::<Vec<_>>(),
            expected
                .into_iter()
                .flat_map(|(_, capabilities)| capabilities.iter().copied())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn attach_environment_is_optional_and_debug_is_redacted() {
        let legacy = r#"{"request":"attach","shell_id":"s1","takeover":false,"profile":{"term":null,"colorterm":null,"term_program":null,"term_program_version":null,"rows":24,"cols":80,"pixel_width":0,"pixel_height":0}}"#;
        let request: Request = serde_json::from_str(legacy).unwrap();
        assert!(matches!(
            request,
            Request::Attach {
                environment: None,
                expected_run_id: None,
                ..
            }
        ));

        let environment = UnixEnvironment {
            variables: vec![UnixEnvironmentVariable {
                name: b"SECRET_NAME".to_vec(),
                value: b"secret-value".to_vec(),
            }],
        };
        let debug = format!("{environment:?}");
        assert!(!debug.contains("SECRET_NAME"));
        assert!(!debug.contains("secret-value"));
        assert!(debug.contains("redacted"));
    }

    #[test]
    fn agent_wait_messages_round_trip() {
        let request = Request::WaitAgent {
            agent_id: "a1".into(),
            after_revision: 3,
            wait_ms: 30_000,
        };
        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(encoded["request"], "wait_agent");
        assert_eq!(serde_json::from_value::<Request>(encoded).unwrap(), request);

        let response = Response::AgentWait {
            agent: AgentInstanceSnapshot {
                id: "a1".into(),
                workspace_id: "w1".into(),
                shell_id: "s1".into(),
                run_id: "r1".into(),
                name: "agent".into(),
                integration: "test".into(),
                external_session_id: Some("external".into()),
                cwd: Some("/tmp/project".into()),
                started_at_ms: 1,
                ended_at_ms: None,
                observation: AgentObservationSnapshot {
                    revision: 4,
                    state: AgentState::Idle,
                    authority: AgentAuthority::LifecycleIntegration,
                    evidence: "idle".into(),
                    confidence: 100,
                    observed_at_ms: 2,
                },
                attention: None,
            },
            changed: true,
        };
        let encoded = serde_json::to_value(&response).unwrap();
        assert_eq!(encoded["response"], "agent_wait");
        assert_eq!(
            serde_json::from_value::<Response>(encoded).unwrap(),
            response
        );
    }

    #[test]
    fn agent_snapshot_defaults_cwd_omitted_by_old_daemons() {
        let agent: AgentInstanceSnapshot = serde_json::from_value(serde_json::json!({
            "id": "a1",
            "workspace_id": "w1",
            "shell_id": "s1",
            "run_id": "r1",
            "name": "agent",
            "integration": "test",
            "external_session_id": "external",
            "started_at_ms": 1,
            "ended_at_ms": null,
            "observation": {
                "revision": 1,
                "state": "working",
                "authority": "lifecycle_integration",
                "evidence": "working",
                "confidence": 100,
                "observed_at_ms": 1
            }
        }))
        .unwrap();

        assert!(agent.cwd.is_none());
        assert!(agent.attention.is_none());
        assert!(serde_json::to_value(agent).unwrap().get("cwd").is_none());
    }

    #[test]
    fn agent_attention_messages_use_snake_case_and_omit_absent_attention() {
        let observation = AgentObservationSnapshot {
            revision: 3,
            state: AgentState::Blocked,
            authority: AgentAuthority::LifecycleIntegration,
            evidence: "needs input".into(),
            confidence: 100,
            observed_at_ms: 4,
        };
        let attention = AgentAttentionSnapshot {
            reason: AgentAttentionReason::Blocked,
            observation: observation.clone(),
        };
        let encoded = serde_json::to_value(&attention).unwrap();
        assert_eq!(encoded["reason"], "blocked");
        assert_eq!(
            serde_json::to_value(AgentAttentionReason::Completed).unwrap(),
            "completed"
        );

        let request = Request::AcknowledgeAgentAttention {
            agent_id: "a1".into(),
            observation_revision: 3,
        };
        assert_eq!(
            serde_json::to_value(&request).unwrap()["request"],
            "acknowledge_agent_attention"
        );
        assert_eq!(
            serde_json::from_value::<Request>(serde_json::to_value(request.clone()).unwrap())
                .unwrap(),
            request
        );

        let mut agent: AgentInstanceSnapshot = serde_json::from_value(serde_json::json!({
            "id": "a1", "workspace_id": "w1", "shell_id": "s1", "run_id": "r1",
            "name": "agent", "integration": "test", "external_session_id": null,
            "started_at_ms": 1, "ended_at_ms": null, "observation": observation
        }))
        .unwrap();
        assert!(agent.attention.is_none());
        assert!(
            serde_json::to_value(&agent)
                .unwrap()
                .get("attention")
                .is_none()
        );
        agent.attention = Some(attention);
        assert_eq!(
            serde_json::to_value(&agent).unwrap()["attention"]["reason"],
            "blocked"
        );
        let response = Response::AgentAttentionAcknowledged {
            agent: agent.clone(),
            changed: true,
        };
        assert_eq!(
            serde_json::to_value(response).unwrap()["response"],
            "agent_attention_acknowledged"
        );
        let event = DaemonEventKind::AgentAttentionAcknowledged {
            workspace_id: "w1".into(),
            shell_id: "s1".into(),
            agent,
        };
        assert_eq!(
            serde_json::to_value(event).unwrap()["event"],
            "agent_attention_acknowledged"
        );
    }

    #[test]
    fn ensure_agent_uses_registration_shape() {
        let request = Request::EnsureAgent {
            shell_id: "s1".into(),
            run_id: "r1".into(),
            spec: AgentRegistrationSpec {
                name: "agent".into(),
                integration: "plugin".into(),
                external_session_id: Some("session-1".into()),
                report: AgentReport {
                    state: AgentState::Working,
                    authority: AgentAuthority::LifecycleIntegration,
                    evidence: "working".into(),
                    confidence: 90,
                },
            },
        };

        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(encoded["request"], "ensure_agent");
        assert_eq!(serde_json::from_value::<Request>(encoded).unwrap(), request);
    }

    #[test]
    fn launcher_request_round_trips_exact_argv() {
        let request = Request::CreateLauncher {
            workspace_id: "w1".into(),
            spec: WorkspaceLauncherSpec {
                name: "editor".into(),
                command: vec!["editor".into(), "".into(), "two words".into()],
                cwd: "/tmp/project".into(),
            },
        };

        let encoded = serde_json::to_vec(&request).unwrap();
        let decoded: Request = serde_json::from_slice(&encoded).unwrap();

        assert_eq!(decoded, request);
    }

    #[test]
    fn protocol_six_client_ignores_additive_shell_snapshot_fields() {
        let snapshot = ShellSnapshot {
            id: "s1".into(),
            revision: 1,
            workspace_id: "w1".into(),
            name: "shell".into(),
            cwd: "/tmp".into(),
            command: vec!["sleep".into(), "1".into()],
            owner: ShellOwner::User,
            status: ShellStatus::Running,
            run: Some(ShellRunSnapshot {
                id: "r1".into(),
                generation: 1,
                started_at_ms: 1,
                ended_at_ms: None,
                exit_reason: None,
                output_revision: 2,
                environment_has_run_id: true,
            }),
            recovered_agent_id: Some("a1".into()),
            foreground_process: Some("sleep".into()),
        };

        let legacy: ProtocolSixShellSnapshot =
            serde_json::from_value(serde_json::to_value(snapshot).unwrap()).unwrap();
        assert_eq!(legacy.id, "s1");
        assert_eq!(legacy.status, ShellStatus::Running);
    }

    #[test]
    fn attach_frames_round_trip() {
        let frames = [
            AttachFrame::Input(vec![0, 1, 255]),
            AttachFrame::Resize {
                rows: 24,
                cols: 80,
                pixel_width: 1920,
                pixel_height: 1080,
            },
            AttachFrame::Detached,
            AttachFrame::Reconnect,
            AttachFrame::ReconnectAck,
            AttachFrame::FocusGained,
        ];
        for frame in frames {
            let mut bytes = Vec::new();
            frame.write_to(&mut bytes).unwrap();
            assert_eq!(
                AttachFrame::read_from(&mut bytes.as_slice()).unwrap(),
                frame
            );
        }
        let mut bytes = Vec::new();
        AttachFrame::FocusGained.write_to(&mut bytes).unwrap();
        assert_eq!(bytes, [7, 0, 0, 0, 0]);
    }

    #[test]
    fn rejects_protocol_two_resize_frame() {
        let bytes = [AttachFrame::RESIZE, 0, 0, 0, 4, 0, 24, 0, 80];

        assert!(AttachFrame::read_from(&mut bytes.as_slice()).is_err());
    }
}
