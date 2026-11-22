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
