use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

/// Local spike protocol only. This is not the remote/E2EE wire contract.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Health {},
    List {},
    Create {
        session_id: Uuid,
        runtime_id: Uuid,
        cwd: PathBuf,
        argv: Vec<String>,
    },
    Snapshot {
        runtime_id: Uuid,
    },
    Attach {
        runtime_id: Uuid,
    },
    Acquire {
        runtime_id: Uuid,
    },
    OpenTerminal {
        runtime_id: Uuid,
        view_id: Uuid,
    },
    AcquireTerminal {
        terminal: TerminalView,
    },
    TerminalStatus {
        terminal: TerminalView,
    },
    ResizeTerminal {
        terminal: TerminalView,
        token: Uuid,
        sequence: u64,
        size: TerminalSize,
    },
    Renew {
        runtime_id: Uuid,
        token: Uuid,
    },
    Release {
        runtime_id: Uuid,
        token: Uuid,
    },
    Input {
        runtime_id: Uuid,
        token: Uuid,
        sequence: u64,
        data: Vec<u8>,
        #[serde(default)]
        paste: bool,
    },
    PasteStage {
        binding: PasteBinding,
        offset: usize,
        data: Vec<u8>,
    },
    PasteCommit {
        binding: PasteBinding,
        bytes: usize,
    },
    PasteAck {
        binding: PasteBinding,
        receipt: Uuid,
    },
    Stop {
        runtime_id: Uuid,
        expected_pane_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Runtime {
    pub session_id: Uuid,
    pub runtime_id: Uuid,
    pub tmux_name: String,
    pub pane_id: String,
    pub command: String,
    pub pane_pid: Option<u32>,
    pub exited: bool,
    pub width: u16,
    pub height: u16,
    #[serde(default)]
    pub origin: RuntimeOrigin,
    #[serde(default)]
    pub workspace_id: Option<Uuid>,
    #[serde(default)]
    pub operation_id: Option<Uuid>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalSize {
    pub cols: u16,
    pub rows: u16,
}

impl TerminalSize {
    pub fn supported(self) -> bool {
        (2..=500).contains(&self.cols) && (2..=200).contains(&self.rows)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalTarget {
    pub runtime_id: Uuid,
    pub generation: Uuid,
    pub session: String,
    pub window: String,
    pub pane: String,
    pub pid: u32,
    pub process_start: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalView {
    pub target: TerminalTarget,
    pub view_id: Uuid,
    pub agent_epoch: Uuid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResizeEpoch {
    pub agent: Uuid,
    pub owner: Uuid,
}

pub const INPUT_LEASE_TTL: std::time::Duration = std::time::Duration::from_secs(15);
pub const PASTE_LIMIT: usize = 65536;
pub const INPUT_CHUNK_LIMIT: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PasteBinding {
    pub terminal: TerminalView,
    pub token: Uuid,
    pub epoch: ResizeEpoch,
    pub incarnation: Uuid,
    pub operation: u64,
    pub sequence: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PasteAdmission {
    PriorSendUnknown,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum PasteResult {
    Staged {},
    Committed { receipt: Uuid },
    Acknowledged { writable: bool },
    Stale {},
    Invalid {},
    Unavailable {},
    Unknown {},
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResizeIntent {
    pub epoch: ResizeEpoch,
    pub id: Uuid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommittedGeometry {
    pub size: TerminalSize,
    pub revision: u64,
    pub intent: Option<ResizeIntent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderSample {
    pub size: TerminalSize,
    pub sampled_at_us: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationState {
    Pending,
    Observed,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalStatus {
    pub terminal: TerminalView,
    pub epoch: Option<ResizeEpoch>,
    pub committed: CommittedGeometry,
    pub provider: Option<ProviderSample>,
    pub observation: ObservationState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResizeResult {
    Committed {
        admission: ResizeIntent,
        status: TerminalStatus,
    },
    Rejected {
        terminal: TerminalView,
        intent: ResizeIntent,
    },
    Uncertain {
        terminal: TerminalView,
        intent: ResizeIntent,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeOrigin {
    #[default]
    Legacy,
    Managed,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    Health {
        agent_pid: u32,
        remote_execution_enabled: bool,
    },
    Runtimes {
        runtimes: Vec<Runtime>,
    },
    Created {
        runtime: Runtime,
    },
    Snapshot {
        text: String,
    },
    Attachment {
        tmux_name: String,
    },
    Control {
        token: Uuid,
    },
    TerminalOpened {
        status: TerminalStatus,
    },
    TerminalControl {
        token: Uuid,
        epoch: ResizeEpoch,
    },
    TerminalStatus {
        status: TerminalStatus,
    },
    TerminalChanged {},
    ControlRecovering {
        paste_admission: PasteAdmission,
    },
    PasteUnavailable {},
    Paste {
        paste: PasteResult,
    },
    Resize {
        resize: ResizeResult,
    },
    Accepted {},
    Stopped {
        runtime_id: Uuid,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderProfile {
    ClaudeCode,
    Codex,
}

impl ProviderProfile {
    pub fn executable_name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude",
            Self::Codex => "codex",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProductIssue {
    InvalidRequest,
    InvalidFolder,
    UnsupportedPath,
    FolderUnavailable,
    FolderAccessDenied,
    WorkspaceUnknown,
    WorkspaceChanged,
    WorkspaceCheckTimedOut,
    ProviderMissing,
    ProviderNotExecutable,
    InterpreterUnavailable,
    CatalogUnconfigured,
    CatalogMissing,
    CatalogUnsafe,
    CatalogBusy,
    CatalogUnsupported,
    CatalogUnavailable,
    CreateConflict,
    RuntimeLimit,
    TmuxUnavailable,
    LaunchSetupFailed,
    PendingUnavailable,
}

impl ProductIssue {
    pub fn storage_fault(self) -> bool {
        matches!(
            self,
            Self::CatalogUnconfigured
                | Self::CatalogMissing
                | Self::CatalogUnsafe
                | Self::CatalogBusy
                | Self::CatalogUnsupported
                | Self::CatalogUnavailable
        )
    }

    pub fn encode(self) -> String {
        serde_json::to_string(&self).expect("product issue is a unit enum")
    }

    pub fn decode(value: &str) -> Option<Self> {
        serde_json::from_str(value).ok()
    }
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDisplay {
    pub workspace_id: Uuid,
    pub label: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProductEnvelope<T> {
    pub product: T,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProductRequest {
    RegisterWorkspace {
        locator: String,
    },
    WorkspaceReadiness {
        workspace_id: Uuid,
        profile: ProviderProfile,
    },
    CreateManaged {
        key: Uuid,
        workspace_id: Uuid,
        profile: ProviderProfile,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reservation: Option<Uuid>,
    },
    CreateStatus {
        key: Uuid,
    },
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProductResponse {
    WorkspaceRegistered { workspace: WorkspaceDisplay },
    Ready {},
    Blocked { issue: ProductIssue },
    Create { state: CreateState },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum CreateState {
    Absent {},
    Pending {
        reservation: Uuid,
    },
    Failed {
        issue: ProductIssue,
    },
    Unknown {
        issue: Option<ProductIssue>,
    },
    Confirmed {
        outcome: LaunchOutcome,
        current: PaneLiveness,
        setup_issue: Option<ProductIssue>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchOutcome {
    pub runtime: Runtime,
    pub setup_issue: Option<ProductIssue>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneLiveness {
    Live,
    Exited,
    Unavailable,
}

pub fn tmux_name(session_id: Uuid, runtime_id: Uuid) -> String {
    format!("rs_{}_{}", session_id.simple(), runtime_id.simple())
}

pub fn parse_name(name: &str) -> Option<(Uuid, Uuid)> {
    let mut parts = name.strip_prefix("rs_")?.split('_');
    let session = parts.next()?;
    let runtime = parts.next()?;
    if session.len() != 32 || runtime.len() != 32 || parts.next().is_some() {
        return None;
    }
    Some((
        Uuid::parse_str(session).ok()?,
        Uuid::parse_str(runtime).ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn managed_names_have_distinct_session_and_runtime_identity() {
        let session = Uuid::new_v4();
        let runtime = Uuid::new_v4();
        assert_eq!(
            parse_name(&tmux_name(session, runtime)),
            Some((session, runtime))
        );
        for invalid in ["default", "rs_*", "rs_;kill-server", "rs_a_b", ""] {
            assert!(parse_name(invalid).is_none());
        }
    }
    #[test]
    fn reject_unknown_operation_and_fields() {
        assert!(serde_json::from_str::<Request>(r#"{"op":"send_input"}"#).is_err());
        assert!(serde_json::from_str::<Request>(r#"{"op":"list","extra":true}"#).is_err());
    }
}
