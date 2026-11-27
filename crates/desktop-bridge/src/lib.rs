//! Local desktop views with host-enforced exclusive input leases.
mod pending_create;
#[cfg(test)]
mod w1_terminal_tests;

use anyhow::{Context, Result, bail, ensure};
pub use pending_create::{
    CreateDisposition, CreateSubmission, LocalCreate, LocalCreateStatus, ProviderReadiness,
};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
pub use remote_sessions_host::encode_local_observation;
pub use remote_sessions_host::model::{
    PasteResult, ProductIssue, ProviderProfile, WorkspaceDisplay,
};
pub use remote_sessions_host::model::{
    ResizeEpoch, ResizeResult, TerminalSize, TerminalStatus, TerminalView,
};
use remote_sessions_host::{
    engine::set_local_pty_nonblocking,
    model::{
        INPUT_CHUNK_LIMIT, INPUT_LEASE_TTL, PasteAdmission, PasteBinding, ProductRequest,
        ProductResponse, Request, Response, Runtime, RuntimeOrigin,
    },
    product_launch::workspace_locator,
    product_request, request,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    io::{ErrorKind, Read},
    os::fd::BorrowedFd,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

#[cfg(test)]
use remote_sessions_host::model::ObservationState;

#[derive(Serialize)]
pub struct Inventory {
    pub agent_pid: u32,
    pub runtimes: Vec<Runtime>,
}
#[derive(Serialize)]
pub struct Attachment {
    pub view_id: String,
    pub cols: u16,
    pub rows: u16,
    pub status: TerminalStatus,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalRestoration {
    previous: TerminalView,
    agent_pid: u32,
    session_id: Uuid,
    operation_id: Option<Uuid>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TerminalInventoryBinding {
    agent_pid: u32,
    session_id: Uuid,
    operation_id: Option<Uuid>,
    workspace_id: Option<Uuid>,
}

impl TerminalInventoryBinding {
    fn current(terminal: &TerminalView, inventory: &Inventory) -> Result<Self> {
        let mut rows = inventory
            .runtimes
            .iter()
            .filter(|runtime| runtime.runtime_id == terminal.target.runtime_id);
        let runtime = rows
            .next()
            .context("Terminal inventory binding unavailable")?;
        ensure!(
            rows.next().is_none()
                && inventory.agent_pid != 0
                && !runtime.session_id.is_nil()
                && runtime.pane_id == terminal.target.pane
                && runtime.pane_pid == Some(terminal.target.pid)
                && !runtime.exited
                && match (runtime.origin, runtime.operation_id, runtime.workspace_id) {
                    (RuntimeOrigin::Legacy, None, None) => true,
                    (RuntimeOrigin::Managed, Some(operation), Some(workspace)) =>
                        !operation.is_nil() && !workspace.is_nil(),
                    _ => false,
                },
            "Terminal inventory binding changed"
        );
        Ok(Self {
            agent_pid: inventory.agent_pid,
            session_id: runtime.session_id,
            operation_id: runtime.operation_id,
            workspace_id: runtime.workspace_id,
        })
    }
}

impl TerminalRestoration {
    fn check_target(&self, terminal: &TerminalView) -> Result<()> {
        ensure!(
            self.previous.agent_epoch == terminal.agent_epoch
                && self.previous.target == terminal.target
                && !self.previous.view_id.is_nil()
                && self.previous.view_id != terminal.view_id,
            "Terminal attachment identity changed"
        );
        Ok(())
    }

    fn check_inventory(&self, binding: TerminalInventoryBinding) -> Result<()> {
        ensure!(
            binding.agent_pid == self.agent_pid
                && binding.session_id == self.session_id
                && binding.operation_id == self.operation_id,
            "Terminal restoration is not corroborated by inventory"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum NativeRejection {
    ViewDetached {
        view_id: Uuid,
        rejection_id: Uuid,
        event_time_us: u64,
        command: TerminalCommand,
        stage: TerminalStage,
    },
    StaleInput {
        view_id: Uuid,
        rejection_id: Uuid,
        event_time_us: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalCommand {
    TerminalInput,
    TerminalRead,
    TerminalAcknowledgeGrid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalStage {
    Input,
    Read,
    Acknowledge,
}

#[derive(Debug)]
struct Detached(NativeRejection);

impl std::fmt::Display for Detached {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("View detached")
    }
}

impl std::error::Error for Detached {}

impl TerminalCommand {
    fn stage(self) -> TerminalStage {
        match self {
            Self::TerminalInput => TerminalStage::Input,
            Self::TerminalRead => TerminalStage::Read,
            Self::TerminalAcknowledgeGrid => TerminalStage::Acknowledge,
        }
    }
}

fn detached(view_id: Uuid, command: TerminalCommand) -> anyhow::Error {
    match sample_clock() {
        Ok(event_time_us) if event_time_us > 0 && !view_id.is_nil() => {
            Detached(NativeRejection::ViewDetached {
                view_id,
                rejection_id: Uuid::new_v4(),
                event_time_us,
                command,
                stage: command.stage(),
            })
            .into()
        }
        _ => anyhow::anyhow!("View detached"),
    }
}

pub fn terminal_rejection(
    error: anyhow::Error,
    diagnostic: Option<bool>,
    trace: bool,
) -> (TerminalInputError, Option<String>) {
    if diagnostic == Some(true)
        && trace
        && let Some(cause) = error.downcast_ref::<Detached>()
        && let NativeRejection::ViewDetached {
            view_id,
            rejection_id,
            event_time_us,
            command,
            stage,
        } = &cause.0
        && !view_id.is_nil()
        && !rejection_id.is_nil()
        && *event_time_us > 0
        && command.stage() == *stage
        && let Ok(observation) = encode_local_observation(&cause.0)
    {
        return (
            TerminalInputError::Rejected(cause.0.clone()),
            Some(observation),
        );
    }
    (TerminalInputError::from(error), None)
}

#[derive(Debug)]
struct StaleInput(NativeRejection);

impl std::fmt::Display for StaleInput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Керування змінилося")
    }
}

impl std::error::Error for StaleInput {}

fn stale_input(view_id: Uuid) -> anyhow::Error {
    match sample_clock() {
        Ok(event_time_us) if (1..=(1 << 53) - 1).contains(&event_time_us) && !view_id.is_nil() => {
            StaleInput(NativeRejection::StaleInput {
                view_id,
                rejection_id: Uuid::new_v4(),
                event_time_us,
            })
            .into()
        }
        _ => anyhow::anyhow!("Керування змінилося"),
    }
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum TerminalInputError {
    Rejected(NativeRejection),
    Message(String),
}

impl From<&str> for TerminalInputError {
    fn from(message: &str) -> Self {
        Self::Message(message.into())
    }
}

impl From<anyhow::Error> for TerminalInputError {
    fn from(error: anyhow::Error) -> Self {
        match error.downcast_ref::<StaleInput>() {
            Some(cause) => Self::Rejected(cause.0.clone()),
            None => Self::Message(error.to_string()),
        }
    }
}
#[derive(Serialize)]
pub struct Chunk {
    pub writable: bool,
    pub data: Vec<u8>,
    pub closed: bool,
    pub status: Option<TerminalStatus>,
    pub geometry: Option<GeometryEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paste_incarnation: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paste_completion: Option<PasteCompletion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paste_admission: Option<PasteAdmission>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closure: Option<TerminalClosure>,
}

impl Chunk {
    fn empty() -> Self {
        Self {
            writable: false,
            data: Vec::new(),
            closed: false,
            status: None,
            geometry: None,
            paste_incarnation: None,
            paste_completion: None,
            paste_admission: None,
            closure: None,
        }
    }

    fn closed(closure: TerminalClosure) -> Self {
        Self {
            closed: true,
            closure: Some(closure),
            ..Self::empty()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum TerminalClosure {
    Overflow { high_water: usize },
    Eof,
    Detached,
    GeometryTimeout,
}

#[derive(Clone, Copy)]
enum Retirement {
    Typed(TerminalClosure),
    Eof,
    ReceiverClosed,
    Failed,
}

const VIEW_LIMIT: usize = 8;
const OUTPUT_SLOTS: usize = 16;
const NOTICE_TTL: Duration = Duration::from_secs(120);
const EOF_TTL: Duration = Duration::from_secs(10);
const ATTACH_PROOF_TIMEOUT: Duration = Duration::from_secs(2);
const WRITER_CANDIDATE_TTL: Duration = Duration::from_secs(10);
const STATUS_REFRESH_INTERVAL: Duration = Duration::from_millis(150);
const CLEANUP_BUDGET: Duration = Duration::from_secs(2);
const CLEANUP_POLL: Duration = Duration::from_millis(5);

#[derive(Default)]
struct Views {
    live: HashMap<Uuid, Arc<View>>,
    writer_candidates: HashMap<Uuid, WriterCandidate>,
    drains: HashMap<Uuid, EofDrain>,
    notices: VecDeque<ClosureNotice>,
    allocated: Arc<AtomicUsize>,
    paste_holds: HashMap<Uuid, PasteAdmission>,
    control_samples: HashMap<Uuid, ControlSample>,
    #[cfg(test)]
    paste_fixture: Option<mpsc::UnboundedSender<PasteFixtureFrame>>,
}

struct WriterCandidate {
    view_id: Uuid,
    allocation: Weak<Allocation>,
    proof: Option<(TerminalInventoryBinding, Instant)>,
}

#[cfg(test)]
struct PasteFixtureFrame {
    request: String,
    reply: tokio::sync::oneshot::Sender<String>,
}

async fn agent_request(
    _views: &Weak<Mutex<Views>>,
    directory: &Path,
    message: &Request,
) -> Result<Response> {
    #[cfg(test)]
    {
        let fixture = _views
            .upgrade()
            .and_then(|views| views.lock().unwrap().paste_fixture.clone());
        if let Some(fixture) = fixture {
            let (reply, receive) = tokio::sync::oneshot::channel();
            fixture
                .send(PasteFixtureFrame {
                    request: encode_local_observation(message)? + "\n",
                    reply,
                })
                .map_err(|_| anyhow::anyhow!("Fixture disconnected"))?;
            let frame = tokio::time::timeout(Duration::from_secs(5), receive).await??;
            return remote_sessions_host::decode_local_fixture(frame.as_bytes());
        }
    }
    request(directory, message).await
}

struct ClosureNotice {
    view: Uuid,
    closure: TerminalClosure,
    expires: Instant,
}

struct EofDrain {
    view: Arc<View>,
    allocation: Arc<Allocation>,
    expires: Instant,
}

impl EofDrain {
    fn discard(self) {
        if let Ok(mut output) = self.view.output.try_lock() {
            output.discard();
            return;
        }
        let Self {
            view, allocation, ..
        } = self;
        let handle = view.handle.clone();
        handle.spawn(async move {
            view.output.lock().await.discard();
            drop(allocation);
        });
    }
}

impl Views {
    fn forget_writer_candidate(&mut self, terminal: &TerminalView) {
        let runtime = terminal.target.runtime_id;
        if self
            .writer_candidates
            .get(&runtime)
            .is_some_and(|candidate| candidate.view_id == terminal.view_id)
        {
            self.writer_candidates.remove(&runtime);
        }
    }

    fn writer_candidate(
        &self,
        terminal: &TerminalView,
        expected: TerminalInventoryBinding,
    ) -> Result<(&Arc<View>, Instant)> {
        let view = self
            .live
            .get(&terminal.view_id)
            .filter(|view| view.terminal == *terminal && !view.closed())
            .context("Terminal candidate is no longer current")?;
        let (_, expires) = self
            .writer_candidates
            .get(&terminal.target.runtime_id)
            .filter(|candidate| candidate.view_id == terminal.view_id)
            .and_then(|candidate| candidate.proof)
            .filter(|(binding, expires)| *binding == expected && *expires > Instant::now())
            .context("Terminal candidate cannot confirm writer control")?;
        Ok((view, expires))
    }

    fn readable_view(&self, id: &Uuid) -> Option<&Arc<View>> {
        self.live
            .get(id)
            .or_else(|| self.drains.get(id).map(|drain| &drain.view))
    }

    fn prune(&mut self, now: Instant) {
        self.notices.retain(|notice| notice.expires > now);
    }

    fn remember(&mut self, view: Uuid, closure: TerminalClosure) {
        let now = Instant::now();
        self.prune(now);
        if self.notices.len() == VIEW_LIMIT {
            self.notices.pop_front();
        }
        self.notices.push_back(ClosureNotice {
            view,
            closure,
            expires: now + NOTICE_TTL,
        });
    }

    fn expire_eof(&mut self, id: Uuid, now: Instant) {
        if let Some(drain) = self.drains.get(&id)
            && drain.expires <= now
        {
            let view = drain.view.clone();
            view.retire_locked(self, Retirement::ReceiverClosed);
        }
    }

    fn reclaim_eof_slot(&mut self) {
        if let Some(drain) = self
            .drains
            .values()
            .filter(|drain| {
                Arc::strong_count(&drain.allocation) == 1
                    && !drain.view.reading.load(Ordering::Relaxed)
            })
            .min_by_key(|drain| drain.expires)
        {
            let view = drain.view.clone();
            view.retire_locked(self, Retirement::ReceiverClosed);
        }
    }

    fn publish_control(&mut self, view: Uuid, control: &Control) {
        let paste_admission = self.paste_holds.get(&view).copied();
        let sample = if paste_admission.is_some() {
            ControlSample {
                lease: None,
                paste_admission,
            }
        } else {
            ControlSample::from_control(control, None)
        };
        self.control_samples.insert(view, sample);
    }

    fn publish_lease(&mut self, view: Uuid, lease: &NativeLease) {
        let paste_admission = self.paste_holds.get(&view).copied();
        let sample = if paste_admission.is_some() {
            ControlSample {
                lease: None,
                paste_admission,
            }
        } else {
            ControlSample {
                lease: Some(LeaseSample::from_lease(lease)),
                paste_admission: None,
            }
        };
        self.control_samples.insert(view, sample);
    }
}

#[derive(Clone, Default)]
struct ControlSample {
    lease: Option<LeaseSample>,
    paste_admission: Option<PasteAdmission>,
}

#[derive(Clone)]
struct LeaseSample {
    epoch: ResizeEpoch,
    deadline: Instant,
    incarnation: Uuid,
    pending_paste: Option<(Uuid, u64)>,
}

struct Allocation(Arc<AtomicUsize>);

impl Drop for Allocation {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

struct AttachmentChild {
    owned: Option<(Box<dyn Child + Send + Sync>, Arc<Allocation>)>,
    handle: tokio::runtime::Handle,
}

impl AttachmentChild {
    fn new(child: Box<dyn Child + Send + Sync>, allocation: Arc<Allocation>) -> Self {
        Self {
            owned: Some((child, allocation)),
            handle: tokio::runtime::Handle::current(),
        }
    }
}

impl Drop for AttachmentChild {
    fn drop(&mut self) {
        if let Some((mut child, allocation)) = self.owned.take() {
            self.handle.spawn_blocking(move || {
                let mut signalled = false;
                while !poll_child(child.as_mut(), &mut signalled) {
                    std::thread::sleep(CLEANUP_POLL);
                }
                drop(allocation);
            });
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct GridSample {
    pub size: TerminalSize,
    pub sampled_at_us: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct GeometryEvent {
    pub status: TerminalStatus,
    pub bridge_pty: GridSample,
    pub client: GridSample,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GridAcknowledgement {
    pub renderer_id: Uuid,
    pub epoch: Option<ResizeEpoch>,
    pub revision: u64,
    pub size: TerminalSize,
    pub focused: bool,
}

pub fn encode_renderer_observation(
    event: &GeometryEvent,
    renderer: GridAcknowledgement,
) -> Result<String> {
    #[derive(Serialize)]
    struct Observation<'a> {
        event: &'a GeometryEvent,
        renderer: GridAcknowledgement,
    }
    encode_local_observation(&Observation { event, renderer })
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum WorkspaceSelection {
    Cancelled {},
    Selected { workspace: WorkspaceDisplay },
    Blocked { issue: ProductIssue },
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum WorkspaceReadiness {
    Ready {},
    Blocked { issue: ProductIssue },
}

struct Control {
    lease: Option<NativeLease>,
}
struct NativeLease {
    token: Uuid,
    epoch: ResizeEpoch,
    input_sequence: u64,
    resize_sequence: u64,
    deadline: Instant,
    incarnation: Uuid,
    pending_paste: Option<NativePaste>,
}

struct NativePaste {
    binding: PasteBinding,
    receipt: Option<Uuid>,
}

#[derive(Serialize)]
pub struct PasteCompletion {
    incarnation: Uuid,
    operation: u64,
}

pub enum PasteAction {
    Stage { offset: usize, data: Vec<u8> },
    Commit { bytes: usize },
    Ack { receipt: String },
}

impl NativeLease {
    fn writable(&self, now: Instant) -> bool {
        self.pending_paste.is_none() && self.deadline > now
    }

    fn extend(&mut self, applied_at: Instant, deadline: Instant) {
        if self.deadline <= applied_at {
            self.incarnation = Uuid::new_v4();
        }
        self.deadline = deadline;
    }
}

impl ControlSample {
    fn from_control(control: &Control, paste_admission: Option<PasteAdmission>) -> Self {
        let lease = control.lease.as_ref().map(LeaseSample::from_lease);
        Self {
            lease,
            paste_admission,
        }
    }
}

impl LeaseSample {
    fn from_lease(lease: &NativeLease) -> Self {
        Self {
            epoch: lease.epoch,
            deadline: lease.deadline,
            incarnation: lease.incarnation,
            pending_paste: lease
                .pending_paste
                .as_ref()
                .map(|pending| (pending.binding.incarnation, pending.binding.operation)),
        }
    }
}
enum PtyEvent {
    Bytes(Arc<Mutex<Vec<u8>>>),
    Boundary,
}
enum PtyRead<'a> {
    Bytes(&'a [u8]),
    Boundary,
}
const RESUME: u64 = 0;
const DRAIN: u64 = 1;
const PAUSE: u64 = 2;
const CLOSE: u64 = 3;
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
const DRAIN_TIMEOUT_ERROR: &str = "Terminal output did not reach a geometry boundary";
const OUTPUT_CHUNK_SIZE: usize = 8192;

#[derive(Clone, Copy, PartialEq, Eq)]
enum VtState {
    Ground,
    Escape,
    EscapeIntermediate,
    CsiEntry,
    CsiParam,
    CsiIntermediate,
    CsiIgnore,
    Osc,
    DcsEntry,
    DcsParam,
    DcsIntermediate,
    DcsIgnore,
    DcsPassthrough,
    SosPmApc,
}

impl VtState {
    fn next(self, code: u32) -> Self {
        use VtState::*;
        // @contract: Follow the xterm 6.0.0 VT500 transition table and string end handlers.
        match code {
            0x1b => return Escape,
            0x90 => return DcsEntry,
            0x98 | 0x9e | 0x9f => return SosPmApc,
            0x9b => return CsiEntry,
            0x9d => return Osc,
            0x18 | 0x1a | 0x80..=0x9f => return Ground,
            0x07 if self == Osc => return Ground,
            0..=0x1f | 0x7f => return self,
            _ => {}
        }
        match (self, code) {
            (Escape, 0x50) => DcsEntry,
            (Escape, 0x58 | 0x5e | 0x5f) => SosPmApc,
            (Escape, 0x5b) => CsiEntry,
            (Escape, 0x5d) => Osc,
            (Escape | EscapeIntermediate, 0x20..=0x2f) => EscapeIntermediate,
            (CsiEntry | CsiParam | CsiIntermediate, 0x20..=0x2f) => CsiIntermediate,
            (CsiEntry, 0x30..=0x3f) | (CsiParam, 0x30..=0x3b) => CsiParam,
            (CsiParam, 0x3c..=0x3f) | (CsiIntermediate, 0x30..=0x3f) => CsiIgnore,
            (CsiIgnore, 0x20..=0x3f | 0xa0..) => CsiIgnore,
            (DcsEntry | DcsParam | DcsIntermediate, 0x20..=0x2f) => DcsIntermediate,
            (DcsEntry, 0x30..=0x3f) | (DcsParam, 0x30..=0x3b) => DcsParam,
            (DcsParam, 0x3c..=0x3f) | (DcsIntermediate, 0x30..=0x3f) => DcsIgnore,
            (DcsEntry | DcsParam | DcsIntermediate, 0x40..=0x7e) => DcsPassthrough,
            (Osc | DcsIgnore | DcsPassthrough, _) | (SosPmApc, 0x20..=0x7e) => self,
            _ => Ground,
        }
    }
}

fn scan_output(vt: &mut VtState, bytes: &[u8]) -> usize {
    // @contract: Keep the structurally incomplete tail, including malformed xterm starters.
    let mut offset = 0;
    'decode: while let Some(&lead) = bytes.get(offset) {
        let start = offset;
        offset += 1;
        let (more, mask, minimum) = match lead {
            0..=0x7f => {
                *vt = vt.next(u32::from(lead));
                continue;
            }
            0xc0..=0xdf => (1, 0x1f, 0x80),
            0xe0..=0xef => (2, 0x0f, 0x800),
            0xf0..=0xf7 => (3, 0x07, 0x10000),
            _ => continue,
        };
        let mut code = u32::from(lead & mask);
        for _ in 0..more {
            match bytes.get(offset).copied() {
                Some(byte @ 0x80..=0xbf) => {
                    code = (code << 6) | u32::from(byte & 0x3f);
                    offset += 1;
                }
                Some(_) => continue 'decode,
                None => return start,
            }
        }
        if code >= minimum && code != 0xfeff && char::from_u32(code).is_some() {
            *vt = vt.next(code);
        }
    }
    offset
}

enum GridBarrier {
    Issued(GeometryEvent),
    Successor(TerminalStatus),
}

struct Output {
    receiver: mpsc::Receiver<PtyEvent>,
    status: TerminalStatus,
    refresh_at: Instant,
    applied_revision: u64,
    pending: Option<GridBarrier>,
    renderer_id: Option<Uuid>,
    drain_started: Option<Instant>,
    writable: bool,
}

impl Output {
    fn discard(&mut self) {
        self.receiver.close();
        while self.receiver.try_recv().is_ok() {}
        self.pending = None;
        self.drain_started = None;
        self.writable = false;
    }

    fn drain_eof(&mut self) -> Result<Chunk> {
        ensure!(
            self.drain_started.is_none()
                && self.pending.is_none()
                && self.status.committed.revision == self.applied_revision,
            DRAIN_TIMEOUT_ERROR
        );
        match self.receiver.try_recv() {
            Ok(PtyEvent::Bytes(data)) => Ok(Chunk {
                data: std::mem::take(&mut *data.lock().unwrap()),
                ..Chunk::empty()
            }),
            Ok(PtyEvent::Boundary) => bail!(DRAIN_TIMEOUT_ERROR),
            Err(_) => Ok(Chunk::closed(TerminalClosure::Eof)),
        }
    }
}

struct View {
    terminal: TerminalView,
    directory: PathBuf,
    tmux: PathBuf,
    client_pid: u32,
    tty: PathBuf,
    control: Arc<tokio::sync::Mutex<Control>>,
    resize: tokio::sync::Mutex<()>,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    output: tokio::sync::Mutex<Output>,
    drain_revision: AtomicU64,
    reader_state: Arc<AtomicU64>,
    views: Weak<Mutex<Views>>,
    allocation: Weak<Allocation>,
    retirement: watch::Sender<Option<Retirement>>,
    reading: AtomicBool,
    handle: tokio::runtime::Handle,
    #[cfg(test)]
    cleanup_confirmations: AtomicU64,
}
impl View {
    async fn agent_request(&self, message: &Request) -> Result<Response> {
        agent_request(&self.views, &self.directory, message).await
    }

    fn hold_paste(&self, hold: PasteAdmission) {
        if let Some(views) = self.views.upgrade() {
            let mut views = views.lock().unwrap();
            if views.live.contains_key(&self.terminal.view_id) {
                views.paste_holds.insert(self.terminal.view_id, hold);
                views.control_samples.insert(
                    self.terminal.view_id,
                    ControlSample {
                        lease: None,
                        paste_admission: Some(hold),
                    },
                );
            }
        }
    }

    fn publish_control(&self, control: &Control) {
        if let Some(views) = self.views.upgrade() {
            let mut views = views.lock().unwrap();
            if views.live.contains_key(&self.terminal.view_id) {
                views.publish_control(self.terminal.view_id, control);
            }
        }
    }

    fn publish_lease(&self, lease: &NativeLease) {
        if let Some(views) = self.views.upgrade() {
            let mut views = views.lock().unwrap();
            if views.live.contains_key(&self.terminal.view_id) {
                views.publish_lease(self.terminal.view_id, lease);
            }
        }
    }

    fn closed(&self) -> bool {
        self.reader_state.load(Ordering::Acquire) == CLOSE
    }

    fn eof_draining(&self) -> bool {
        matches!(*self.retirement.borrow(), Some(Retirement::Eof))
    }

    fn publish(&self, publisher: &mut PtyPublisher, event: PtyRead<'_>) -> bool {
        let Some(registry) = self.views.upgrade() else {
            return false;
        };
        let mut views = registry.lock().unwrap();
        if self.closed() {
            return false;
        }
        let Some(reason) = publisher.publish(event) else {
            return true;
        };
        let at = self.retire_locked(&mut views, reason);
        drop(views);
        if let Retirement::Typed(TerminalClosure::Overflow { high_water }) = reason {
            self.trace_pressure_at("full", Some(high_water), at);
        }
        false
    }

    fn retire(&self, reason: Retirement) {
        let Some(registry) = self.views.upgrade() else {
            return;
        };
        let mut views = registry.lock().unwrap();
        let at = self.retire_locked(&mut views, reason);
        drop(views);
        if matches!(reason, Retirement::Typed(TerminalClosure::Detached)) {
            self.trace_pressure_at("detached", None, at);
        }
    }

    fn retire_locked(&self, views: &mut Views, reason: Retirement) -> Option<u64> {
        let id = self.terminal.view_id;
        let reason = if matches!(reason, Retirement::Eof) {
            if !views.live.contains_key(&id) {
                return None;
            }
            match self.reader_state.swap(CLOSE, Ordering::AcqRel) {
                DRAIN | PAUSE => Retirement::Typed(TerminalClosure::GeometryTimeout),
                _ => reason,
            }
        } else {
            reason
        };
        if matches!(reason, Retirement::Eof) {
            let allocation = self.allocation.upgrade()?;
            let view = views.live.remove(&id)?;
            let expires = Instant::now() + EOF_TTL;
            views.drains.insert(
                id,
                EofDrain {
                    view: view.clone(),
                    allocation,
                    expires,
                },
            );
            self.handle.spawn(async move {
                tokio::select! {
                    _ = view.discarded() => {},
                    _ = tokio::time::sleep_until(expires.into()) => {
                        if let Some(views) = view.views.upgrade() {
                            views.lock().unwrap().expire_eof(id, Instant::now());
                        }
                    }
                }
            });
        } else if let Some(drain) = views.drains.remove(&id) {
            drain.discard();
        } else {
            views.live.remove(&id)?;
        }
        views.paste_holds.remove(&id);
        views.control_samples.remove(&id);
        views.forget_writer_candidate(&self.terminal);
        self.reader_state.store(CLOSE, Ordering::Release);
        if !self.reading.load(Ordering::Relaxed)
            && let Retirement::Typed(closure) = reason
            && closure != TerminalClosure::Detached
        {
            views.remember(id, closure);
        }
        let at = sample_clock().ok();
        self.retirement.send_replace(Some(reason));
        at
    }

    async fn retired(&self) {
        let mut retirement = self.retirement.subscribe();
        let _ = retirement.wait_for(Option::is_some).await;
    }

    async fn discarded(&self) {
        let mut retirement = self.retirement.subscribe();
        let _ = retirement
            .wait_for(|reason| matches!(reason, Some(reason) if !matches!(reason, Retirement::Eof)))
            .await;
    }

    fn trace_pressure(&self, event: &str) {
        self.trace_pressure_at(event, None, sample_clock().ok());
    }

    fn trace_pressure_at(&self, event: &str, high_water: Option<usize>, at: Option<u64>) {
        if cfg!(debug_assertions) && std::env::var("RS_W1_RESIZE_TRACE").as_deref() == Ok("1") {
            use std::io::Write;
            let view = self.terminal.view_id;
            let pid = self.client_pid;
            if let Some(at_us) = at {
                let depth =
                    high_water.map_or(String::new(), |depth| format!(",\"high_water\":{depth}"));
                let _ = writeln!(
                    std::io::stderr().lock(),
                    "RS_W1_RESIZE_PRESSURE {{\"event\":\"{event}\",\"view_id\":\"{view}\",\"pid\":{pid},\"at_us\":{at_us}{depth}}}"
                );
            }
        }
    }

    async fn release(&self, token: Uuid) {
        let _ = tokio::time::timeout(
            Duration::from_secs(1),
            self.agent_request(&Request::Release {
                runtime_id: self.terminal.target.runtime_id,
                token,
            }),
        )
        .await;
    }
}

struct PendingRead<'a> {
    view: &'a View,
    views: &'a Mutex<Views>,
    finished: bool,
}

impl PendingRead<'_> {
    fn finish(mut self, result: Result<Chunk>) -> Result<Chunk> {
        let mut views = self.views.lock().unwrap();
        self.view.reading.store(false, Ordering::Relaxed);
        self.finished = true;
        let retirement = *self.view.retirement.borrow();
        match retirement {
            Some(Retirement::Typed(closure)) => Ok(Chunk::closed(closure)),
            Some(Retirement::Eof) => {
                let chunk = result?;
                if chunk.closure == Some(TerminalClosure::Eof) {
                    self.view
                        .retire_locked(&mut views, Retirement::ReceiverClosed);
                    return Ok(chunk);
                }
                Ok(Chunk {
                    data: chunk.data,
                    ..Chunk::empty()
                })
            }
            Some(_) if result.is_ok() => Err(detached(
                self.view.terminal.view_id,
                TerminalCommand::TerminalRead,
            )),
            _ => result,
        }
    }
}

impl Drop for PendingRead<'_> {
    fn drop(&mut self) {
        if !self.finished {
            let mut views = self.views.lock().unwrap();
            self.view.reading.store(false, Ordering::Relaxed);
            if let Some(Retirement::Typed(closure)) = *self.view.retirement.borrow()
                && closure != TerminalClosure::Detached
            {
                views.remember(self.view.terminal.view_id, closure);
            }
        }
    }
}

fn supervise(
    view: Arc<View>,
    mut child: AttachmentChild,
    reader: Option<std::thread::JoinHandle<()>>,
) {
    let (mut child, allocation) = child.owned.take().unwrap();
    view.handle.block_on(view.retired());
    let deadline = Instant::now() + CLEANUP_BUDGET;
    view.master.lock().unwrap().take();
    let output_cleanup = (!view.eof_draining()).then(|| {
        let cleanup = view.clone();
        view.handle.spawn(async move {
            cleanup.output.lock().await.discard();
        })
    });

    let cleanup = view.clone();
    view.handle.spawn(async move {
        let token = cleanup
            .control
            .lock()
            .await
            .lease
            .take()
            .map(|lease| lease.token);
        if let Some(token) = token {
            cleanup.release(token).await;
        }
    });
    let mut signalled = false;
    let mut reap = false;
    let mut reader = reader;
    let mut reported_timeout = false;
    loop {
        if reader.as_ref().is_some_and(|reader| reader.is_finished()) {
            let _ = reader.take().unwrap().join();
            #[cfg(test)]
            view.cleanup_confirmations.fetch_or(1, Ordering::Release);
            view.trace_pressure("reader_exit");
        }
        if !reap && poll_child(child.as_mut(), &mut signalled) {
            reap = true;
            #[cfg(test)]
            view.cleanup_confirmations.fetch_or(2, Ordering::Release);
            view.trace_pressure("child_reaped");
        }
        if reap && reader.is_none() && output_cleanup.as_ref().is_none_or(|job| job.is_finished()) {
            drop(allocation);
            view.trace_pressure("cleanup_complete");
            return;
        }
        if !reported_timeout && Instant::now() >= deadline {
            reported_timeout = true;
            view.trace_pressure("cleanup_timeout");
        }
        std::thread::sleep(CLEANUP_POLL);
    }
}

fn poll_child(child: &mut dyn Child, signalled: &mut bool) -> bool {
    match child.try_wait() {
        Ok(Some(_)) => return true,
        Ok(None) if !*signalled => {
            *signalled = true;
            let _ = child.kill();
        }
        _ => {}
    }
    false
}

fn pty_size(size: TerminalSize) -> PtySize {
    PtySize {
        cols: size.cols,
        rows: size.rows,
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn sample_clock() -> Result<u64> {
    Ok(u64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_micros(),
    )?)
}

fn sample_master(master: &dyn MasterPty) -> Result<GridSample> {
    let actual = master.get_size()?;
    Ok(GridSample {
        size: TerminalSize {
            cols: actual.cols,
            rows: actual.rows,
        },
        sampled_at_us: sample_clock()?,
    })
}

struct PtyPublisher {
    sender: mpsc::Sender<PtyEvent>,
    tail: Weak<Mutex<Vec<u8>>>,
}

impl PtyPublisher {
    fn new(sender: mpsc::Sender<PtyEvent>) -> Self {
        Self {
            sender,
            tail: Weak::new(),
        }
    }

    fn publish(&mut self, read: PtyRead<'_>) -> Option<Retirement> {
        let event = match read {
            PtyRead::Bytes(bytes) => {
                if self.sender.is_closed() {
                    return Some(Retirement::ReceiverClosed);
                }
                // @invariant: The consumer empties a claimed chunk under the same lock.
                if let Some(tail) = self.tail.upgrade()
                    && let Ok(mut queued) = tail.try_lock()
                    && !queued.is_empty()
                    && queued.len() + bytes.len() <= OUTPUT_CHUNK_SIZE
                {
                    queued.reserve_exact(bytes.len());
                    queued.extend_from_slice(bytes);
                    return None;
                }
                let queued = Arc::new(Mutex::new(bytes.to_vec()));
                self.tail = Arc::downgrade(&queued);
                PtyEvent::Bytes(queued)
            }
            PtyRead::Boundary => {
                self.tail = Weak::new();
                PtyEvent::Boundary
            }
        };
        match self.sender.try_send(event) {
            Ok(()) => None,
            Err(mpsc::error::TrySendError::Full(_)) => {
                Some(Retirement::Typed(TerminalClosure::Overflow {
                    high_water: self.sender.max_capacity(),
                }))
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Some(Retirement::ReceiverClosed),
        }
    }
}

fn read_pty(
    mut reader: Box<dyn Read + Send>,
    state: &AtomicU64,
    mut publish: impl FnMut(PtyRead<'_>) -> bool,
    retire: impl FnOnce(Retirement),
) {
    let mut bytes = [0_u8; OUTPUT_CHUNK_SIZE];
    let mut carry = 0;
    let mut vt = VtState::Ground;
    loop {
        match state.load(Ordering::Acquire) {
            CLOSE => return,
            PAUSE => std::thread::sleep(CLEANUP_POLL),
            _ => match reader.read(&mut bytes[carry..]) {
                Ok(0) => break,
                Ok(count) => {
                    if state.load(Ordering::Acquire) == CLOSE {
                        return;
                    }
                    let end = carry + count;
                    let complete = scan_output(&mut vt, &bytes[..end]);
                    if complete != 0 && !publish(PtyRead::Bytes(&bytes[..complete])) {
                        return;
                    }
                    bytes.copy_within(complete..end, 0);
                    carry = end - complete;
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    if carry == 0
                        && vt == VtState::Ground
                        && state
                            .compare_exchange(DRAIN, PAUSE, Ordering::AcqRel, Ordering::Acquire)
                            .is_ok()
                        && !publish(PtyRead::Boundary)
                    {
                        return;
                    }
                    std::thread::sleep(CLEANUP_POLL);
                }
                Err(_) => return retire(Retirement::Failed),
            },
        }
    }
    retire(Retirement::Eof);
}

async fn refresh_control(view: &View) {
    let mut control = view.control.lock().await;
    if view.closed() {
        return;
    }
    let message = match &control.lease {
        Some(lease) if lease.pending_paste.is_some() => return,
        Some(lease) => Request::Renew {
            runtime_id: view.terminal.target.runtime_id,
            token: lease.token,
        },
        None => Request::AcquireTerminal {
            terminal: view.terminal.clone(),
        },
    };
    let deadline = Instant::now() + INPUT_LEASE_TTL;
    let result = view.agent_request(&message).await;
    {
        let views = view.views.upgrade();
        let mut registry = views.as_ref().map(|views| views.lock().unwrap());
        let applied_at = Instant::now();
        if !view.closed() && applied_at < deadline {
            match &result {
                Ok(Response::TerminalControl { token, epoch })
                    if epoch.agent == view.terminal.agent_epoch =>
                {
                    if let Some(lease) = control
                        .lease
                        .as_mut()
                        .filter(|lease| lease.token == *token && lease.epoch == *epoch)
                    {
                        lease.extend(applied_at, deadline);
                    } else {
                        control.lease = Some(NativeLease {
                            token: *token,
                            epoch: *epoch,
                            deadline,
                            input_sequence: 0,
                            resize_sequence: 0,
                            incarnation: Uuid::new_v4(),
                            pending_paste: None,
                        });
                    }
                    if let Some(registry) = registry.as_mut() {
                        registry.paste_holds.remove(&view.terminal.view_id);
                        registry.publish_control(view.terminal.view_id, &control);
                    }
                    return;
                }
                Ok(Response::Accepted {}) if let Some(lease) = control.lease.as_mut() => {
                    lease.extend(applied_at, deadline);
                    if let Some(registry) = registry.as_mut() {
                        registry.publish_control(view.terminal.view_id, &control);
                    }
                    return;
                }
                _ => {}
            }
        }
    }
    match result {
        Ok(Response::TerminalControl { token, .. }) => {
            drop(control);
            view.release(token).await;
        }
        result => {
            if let Ok(Response::ControlRecovering { paste_admission }) = result {
                view.hold_paste(paste_admission);
            }
            let token = control.lease.take().map(|lease| lease.token);
            view.publish_control(&control);
            drop(control);
            if let Some(token) = token {
                view.release(token).await;
            }
        }
    }
}

fn start_control(view: &Arc<View>) {
    let active = Arc::downgrade(view);
    tokio::spawn(async move {
        while let Some(view) = active.upgrade() {
            if view.closed() {
                break;
            }
            refresh_control(&view).await;
            tokio::select! {
                _ = view.retired() => break,
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        }
    });
}

async fn inventory(views: &Weak<Mutex<Views>>, directory: &Path) -> Result<Inventory> {
    let Response::Health { agent_pid, .. } =
        agent_request(views, directory, &Request::Health {}).await?
    else {
        bail!("Agent health unavailable");
    };
    match agent_request(views, directory, &Request::List {}).await? {
        Response::Runtimes { runtimes } => Ok(Inventory {
            agent_pid,
            runtimes,
        }),
        Response::Error { message } => bail!("{message}"),
        _ => bail!("Unexpected inventory response"),
    }
}

enum AttachmentMode {
    Ordinary,
    Observer,
    Candidate,
    Restore(TerminalRestoration),
}

impl AttachmentMode {
    fn is_candidate(&self) -> bool {
        matches!(self, Self::Candidate | Self::Restore(_))
    }

    fn check_opened(&self, opened: &TerminalView) -> Result<()> {
        if let Self::Restore(expected) = self {
            expected.check_target(opened)?;
        }
        Ok(())
    }

    async fn prepare_control(
        &self,
        view: &View,
        proof: &TerminalStatus,
    ) -> Result<Option<TerminalInventoryBinding>> {
        if !self.is_candidate() {
            return Ok(None);
        }
        self.check_opened(&proof.terminal)?;
        ensure!(
            view.terminal == proof.terminal,
            "Terminal attachment identity changed"
        );
        let current = inventory(&view.views, &view.directory).await?;
        let binding = TerminalInventoryBinding::current(&proof.terminal, &current)?;
        if let Self::Restore(expected) = self {
            expected.check_inventory(binding)?;
        }
        Ok(Some(binding))
    }
}

async fn terminal_status(view: &View) -> Result<TerminalStatus> {
    match request(
        &view.directory,
        &Request::TerminalStatus {
            terminal: view.terminal.clone(),
        },
    )
    .await?
    {
        Response::TerminalStatus { status } if status.terminal == view.terminal => Ok(status),
        Response::TerminalStatus { .. } | Response::TerminalChanged {} => {
            view.retire(Retirement::Failed);
            bail!("Terminal attachment identity changed")
        }
        _ => bail!("Розмір термінала недоступний"),
    }
}

fn drain_budget(started: Option<Instant>, now: Instant) -> Result<Option<Duration>> {
    let remaining = started.map(|start| (start + DRAIN_TIMEOUT).saturating_duration_since(now));
    ensure!(
        remaining.is_none_or(|time| !time.is_zero()),
        DRAIN_TIMEOUT_ERROR
    );
    Ok(remaining)
}

async fn terminal_status_during_drain(
    view: &View,
    remaining: Option<Duration>,
) -> Result<TerminalStatus> {
    match remaining {
        Some(time) => tokio::time::timeout(time, terminal_status(view))
            .await
            .context(DRAIN_TIMEOUT_ERROR)?,
        None => terminal_status(view).await,
    }
}

async fn view_command(view: &View, args: &[&str]) -> Result<String> {
    let output = tokio::time::timeout(
        Duration::from_millis(400),
        tokio::process::Command::new(&view.tmux)
            .args(["-N", "-S"])
            .arg(view.directory.join("tmux.sock"))
            .args(args)
            .env_remove("TMUX")
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    ensure!(
        output.status.success() && output.stdout.len() <= 65_536,
        "Attachment client unavailable"
    );
    Ok(String::from_utf8(output.stdout)?)
}

async fn client_grid(view: &View) -> Result<GridSample> {
    let target = &view.terminal.target;
    let rows = view_command(view, &[
        "list-clients", "-t", &target.session, "-F",
        "#{client_pid}|#{client_tty}|#{session_id}|#{window_id}|#{pane_id}|#{@rs_create_generation}|#{client_flags}|#{client_width}|#{client_height}",
    ]).await?;
    let pid = view.client_pid.to_string();
    let generation = target.generation.to_string();
    let mut matches = rows.lines().filter_map(|line| {
        let fields: Vec<_> = line.split('|').collect();
        let [
            candidate,
            tty,
            session,
            window,
            pane,
            server,
            flags,
            cols,
            rows,
        ] = fields.as_slice()
        else {
            return None;
        };
        (*candidate == pid.as_str()
            && Path::new(*tty) == view.tty.as_path()
            && *session == target.session.as_str()
            && *window == target.window.as_str()
            && *pane == target.pane.as_str()
            && *server == generation.as_str()
            && flags.split(',').any(|flag| flag == "ignore-size"))
        .then_some((*cols, *rows))
    });
    let (cols, rows) = matches
        .next()
        .context("Exact tmux attachment client unavailable")?;
    ensure!(matches.next().is_none(), "Ambiguous attachment client");
    let size = TerminalSize {
        cols: cols.parse()?,
        rows: rows.parse()?,
    };
    ensure!(size.supported(), "Invalid tmux client grid");
    Ok(GridSample {
        size,
        sampled_at_us: sample_clock()?,
    })
}

async fn prove_attachment(view: &View, opened: &TerminalStatus) -> Result<TerminalStatus> {
    ensure!(
        view.client_pid != 0,
        "Attachment client identity unavailable"
    );
    tokio::time::timeout(ATTACH_PROOF_TIMEOUT, async {
        loop {
            if let Ok(grid) = client_grid(view).await
                && grid.size == opened.committed.size
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let status = terminal_status(view).await?;
        ensure!(
            status.committed == opened.committed,
            "Terminal geometry changed during attachment proof"
        );
        ensure!(
            client_grid(view).await?.size == status.committed.size,
            "Attachment client grid changed during proof"
        );
        Ok(status)
    })
    .await
    .context("Terminal attachment proof timed out")?
}

async fn prepare_grid(view: &View, status: TerminalStatus) -> Result<GeometryEvent> {
    let size = status.committed.size;
    let bridge_pty = {
        let master = view
            .master
            .lock()
            .map_err(|_| anyhow::anyhow!("Native PTY unavailable"))?;
        let master = master.as_ref().context("View detached")?;
        master.resize(pty_size(size))?;
        sample_master(master.as_ref())?
    };
    ensure!(
        bridge_pty.size == size,
        "Native PTY resize was not observed"
    );
    let client = tokio::time::timeout(Duration::from_millis(800), async {
        loop {
            if let Ok(sample) = client_grid(view).await
                && sample.size == size
            {
                return sample;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("Tmux client resize was not observed")?;
    Ok(GeometryEvent {
        status,
        bridge_pty,
        client,
    })
}

fn newer_geometry(issued: &TerminalStatus, current: &TerminalStatus) -> Result<bool> {
    if current.committed.revision > issued.committed.revision {
        return Ok(true);
    }
    ensure!(
        current.epoch == issued.epoch && current.committed == issued.committed,
        "Terminal geometry boundary was superseded"
    );
    Ok(false)
}

async fn redraw_view(
    view: &View,
    boundary: &GeometryEvent,
) -> Result<(GeometryEvent, Option<TerminalStatus>)> {
    let current = terminal_status(view).await?;
    let (status, successor) = if newer_geometry(&boundary.status, &current)? {
        (boundary.status.clone(), Some(current))
    } else {
        (current, None)
    };
    let (bridge_pty, _tty_guard) = {
        let master = view
            .master
            .lock()
            .map_err(|_| anyhow::anyhow!("Native PTY unavailable"))?;
        let master = master.as_ref().context("View detached")?;
        let fd = master
            .as_raw_fd()
            .context("Attachment PTY descriptor unavailable")?;
        let pinned = unsafe { BorrowedFd::borrow_raw(fd) }.try_clone_to_owned()?;
        (sample_master(master.as_ref())?, pinned)
    };
    let client = client_grid(view).await?;
    let size = status.committed.size;
    ensure!(
        !view.closed() && bridge_pty.size == size && client.size == size,
        "Attachment grid changed"
    );
    if successor.is_none() {
        view_command(
            view,
            &[
                "refresh-client",
                "-t",
                view.tty.to_str().context("Invalid attachment TTY")?,
            ],
        )
        .await?;
    }
    Ok((
        GeometryEvent {
            status,
            bridge_pty,
            client,
        },
        successor,
    ))
}

fn expire_drain(registry: &Mutex<Views>, id: Uuid, revision: u64) {
    let mut views = registry.lock().unwrap();
    if let Some(view) = views.readable_view(&id).cloned()
        && view.drain_revision.load(Ordering::Relaxed) == revision
    {
        view.retire_locked(
            &mut views,
            Retirement::Typed(TerminalClosure::GeometryTimeout),
        );
    }
}

async fn issue_grid(
    view: &View,
    output: &mut Output,
    status: TerminalStatus,
    remaining: Duration,
    now: &impl Fn() -> Instant,
) -> Result<GeometryEvent> {
    let event = tokio::time::timeout(remaining, prepare_grid(view, status))
        .await
        .context(DRAIN_TIMEOUT_ERROR)??;
    drain_budget(output.drain_started, now())?;
    output.status = event.status.clone();
    output.pending = Some(GridBarrier::Issued(event.clone()));
    Ok(event)
}

async fn poll_output(
    view: &View,
    output: &mut Output,
    now: &impl Fn() -> Instant,
) -> Result<Chunk> {
    let mut chunk = Chunk::empty();
    let reading_at = now();
    let remaining = drain_budget(output.drain_started, reading_at)?;
    let refresh = if reading_at >= output.refresh_at {
        Some(terminal_status_during_drain(view, remaining).await)
    } else {
        None
    };
    let draining_at = {
        let registry = refresh.as_ref().and_then(|_| view.views.upgrade());
        let views = registry.as_ref().map(|registry| registry.lock().unwrap());
        if let Some(status) = refresh {
            drain_budget(output.drain_started, now())?;
            if view.eof_draining() {
                ensure!(
                    views
                        .as_ref()
                        .and_then(|views| views.drains.get(&view.terminal.view_id))
                        .is_some_and(|drain| {
                            std::ptr::eq(drain.view.as_ref(), view)
                                && drain.expires > Instant::now()
                        }),
                    "View detached"
                );
                return output.drain_eof();
            }
            let status = status?;
            ensure!(
                status.committed.revision >= output.status.committed.revision,
                "Terminal geometry revision went backwards"
            );
            output.status = status;
            output.refresh_at = now() + STATUS_REFRESH_INTERVAL;
            chunk.status = Some(output.status.clone());
        }
        let draining_at = now();
        if output.status.committed.revision > output.applied_revision {
            match view.reader_state.compare_exchange(
                RESUME,
                DRAIN,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    output.drain_started = Some(draining_at);
                    let revision = output.status.committed.revision;
                    view.drain_revision.store(revision, Ordering::Relaxed);
                    let deadline = draining_at + DRAIN_TIMEOUT;
                    let active = view.views.clone();
                    let id = view.terminal.view_id;
                    view.handle.spawn(async move {
                        tokio::time::sleep_until(deadline.into()).await;
                        if let Some(views) = active.upgrade() {
                            expire_drain(&views, id, revision);
                        }
                    });
                }
                Err(CLOSE) => bail!(DRAIN_TIMEOUT_ERROR),
                Err(_) => {}
            }
        }
        draining_at
    };
    let poll = Duration::from_millis(150);
    let wait = drain_budget(output.drain_started, draining_at)?
        .unwrap_or(poll)
        .min(poll);
    let event = tokio::select! {
        biased;
        _ = view.retired() => {
            ensure!(view.eof_draining(), "View detached");
            return output.drain_eof();
        }
        event = tokio::time::timeout(wait, output.receiver.recv()) => event,
    };
    let remaining = drain_budget(output.drain_started, now())?;
    match event {
        Ok(Some(PtyEvent::Bytes(data))) => chunk.data = std::mem::take(&mut *data.lock().unwrap()),
        Ok(Some(PtyEvent::Boundary)) => {
            ensure!(!view.eof_draining(), DRAIN_TIMEOUT_ERROR);
            let status = terminal_status_during_drain(view, remaining).await?;
            let remaining =
                drain_budget(output.drain_started, now())?.context(DRAIN_TIMEOUT_ERROR)?;
            ensure!(
                status.committed.revision >= output.status.committed.revision,
                "Terminal geometry revision went backwards"
            );
            chunk.geometry = Some(issue_grid(view, output, status, remaining, now).await?);
            chunk.status = None;
        }
        Ok(None) if view.eof_draining() => return output.drain_eof(),
        Ok(None) => {
            view.retire(Retirement::ReceiverClosed);
        }
        Err(_) if view.eof_draining() => return output.drain_eof(),
        Err(_) => {}
    }
    Ok(chunk)
}

async fn read_chunk(view: &View, output: &mut Output, now: impl Fn() -> Instant) -> Result<Chunk> {
    if view.eof_draining() {
        return output.drain_eof();
    }
    let mut chunk = match &output.pending {
        Some(GridBarrier::Successor(successor)) => {
            let remaining = drain_budget(output.drain_started, now())?;
            let current = terminal_status_during_drain(view, remaining).await?;
            let remaining =
                drain_budget(output.drain_started, now())?.context(DRAIN_TIMEOUT_ERROR)?;
            let status = if newer_geometry(successor, &current)? {
                successor.clone()
            } else {
                current
            };
            Chunk {
                geometry: Some(issue_grid(view, output, status, remaining, &now).await?),
                ..Chunk::empty()
            }
        }
        Some(GridBarrier::Issued(_)) => bail!("Terminal grid acknowledgement required"),
        None => poll_output(view, output, &now).await?,
    };
    let control_sample = view.views.upgrade().and_then(|views| {
        let views = views.lock().unwrap();
        (!view.closed())
            .then(|| views.control_samples.get(&view.terminal.view_id).cloned())
            .flatten()
    });
    if let Some(sample) = control_sample {
        chunk.paste_admission = sample.paste_admission;
        if chunk.paste_admission.is_none()
            && let Some(lease) = sample.lease
        {
            if let Some((incarnation, operation)) = lease.pending_paste {
                chunk.paste_completion = Some(PasteCompletion {
                    incarnation,
                    operation,
                });
            } else if output.status.terminal == view.terminal
                && Some(lease.epoch) == output.status.epoch
                && lease.deadline > now()
            {
                chunk.paste_incarnation = Some(lease.incarnation);
            }
        }
    }
    output.writable = chunk.paste_incarnation.is_some();
    chunk.writable = output.writable;
    Ok(chunk)
}

pub struct DesktopBridge {
    generation: AtomicU64,
    directory: Option<PathBuf>,
    tmux: PathBuf,
    views: Arc<Mutex<Views>>,
    creates: tokio::sync::Mutex<()>,
    #[cfg(test)]
    startup_gate: Mutex<Option<w1_terminal_tests::ViewGate>>,
    #[cfg(test)]
    grid_completion_gate: Mutex<Option<w1_terminal_tests::ViewGate>>,
}
impl DesktopBridge {
    pub fn new(directory: Option<PathBuf>, tmux: PathBuf) -> Self {
        Self {
            generation: AtomicU64::new(0),
            directory,
            tmux,
            views: Arc::new(Mutex::new(Views::default())),
            creates: tokio::sync::Mutex::new(()),
            #[cfg(test)]
            startup_gate: Mutex::new(None),
            #[cfg(test)]
            grid_completion_gate: Mutex::new(None),
        }
    }
    fn directory(&self) -> Result<&PathBuf> {
        self.directory
            .as_ref()
            .context("Локальний агент не налаштований. Запустіть desktop lab.")
    }
    pub async fn pick_workspace(&self, picked: Option<PathBuf>) -> Result<WorkspaceSelection> {
        let Some(path) = picked else {
            return Ok(WorkspaceSelection::Cancelled {});
        };
        let locator = match workspace_locator(&path) {
            Ok(locator) => locator.to_owned(),
            Err(issue) => return Ok(WorkspaceSelection::Blocked { issue }),
        };
        match product_request(
            self.directory()?,
            &ProductRequest::RegisterWorkspace { locator },
        )
        .await?
        {
            ProductResponse::WorkspaceRegistered { workspace } => {
                ensure!(
                    !workspace.workspace_id.is_nil()
                        && !workspace.label.is_empty()
                        && workspace.label.len() <= 4096
                        && !workspace.label.chars().any(char::is_control),
                    "Invalid workspace display metadata"
                );
                Ok(WorkspaceSelection::Selected { workspace })
            }
            ProductResponse::Blocked { issue } => Ok(WorkspaceSelection::Blocked { issue }),
            _ => bail!("Unexpected workspace selection response"),
        }
    }

    pub async fn workspace_readiness(
        &self,
        workspace_id: &str,
        profile: ProviderProfile,
    ) -> Result<WorkspaceReadiness> {
        let Ok(workspace_id) = Uuid::parse_str(workspace_id) else {
            return Ok(WorkspaceReadiness::Blocked {
                issue: ProductIssue::InvalidRequest,
            });
        };
        let request = ProductRequest::WorkspaceReadiness {
            workspace_id,
            profile,
        };
        match product_request(self.directory()?, &request).await? {
            ProductResponse::Ready {} => Ok(WorkspaceReadiness::Ready {}),
            ProductResponse::Blocked { issue } => Ok(WorkspaceReadiness::Blocked { issue }),
            _ => bail!("Unexpected readiness response"),
        }
    }
    pub async fn inventory(&self) -> Result<Inventory> {
        inventory(&Arc::downgrade(&self.views), self.directory()?).await
    }
    pub async fn attach(&self, runtime_id: &str) -> Result<Attachment> {
        self.attach_view(runtime_id, AttachmentMode::Ordinary).await
    }

    pub async fn attach_observer(&self, runtime_id: &str) -> Result<Attachment> {
        self.attach_view(runtime_id, AttachmentMode::Observer).await
    }

    pub async fn prepare_attachment(&self, runtime_id: &str) -> Result<Attachment> {
        self.attach_view(runtime_id, AttachmentMode::Candidate)
            .await
    }

    pub async fn restore_attachment(
        &self,
        runtime_id: &str,
        restoration: TerminalRestoration,
    ) -> Result<Attachment> {
        self.attach_view(runtime_id, AttachmentMode::Restore(restoration))
            .await
    }

    pub async fn confirm_writer(
        &self,
        terminal: TerminalView,
        expected: TerminalInventoryBinding,
    ) -> Result<()> {
        let generation = self.generation.load(Ordering::SeqCst);
        let (view, expires) = {
            let views = self.views.lock().unwrap();
            let (view, expires) = views.writer_candidate(&terminal, expected)?;
            (view.clone(), expires)
        };
        let corroborate = async {
            let current = inventory(&view.views, &view.directory).await?;
            ensure!(
                TerminalInventoryBinding::current(&terminal, &current)? == expected,
                "Terminal inventory binding changed"
            );
            client_grid(&view).await?;
            terminal_status(&view).await?;
            Ok::<(), anyhow::Error>(())
        };
        tokio::select! {
            biased;
            _ = view.retired() => bail!("Terminal candidate is no longer current"),
            proof = tokio::time::timeout_at(expires.into(), corroborate) => {
                proof.context("Terminal candidate expired")??;
            }
        }
        let mut views = self.views.lock().unwrap();
        let (current, _) = views.writer_candidate(&terminal, expected)?;
        ensure!(
            generation == self.generation.load(Ordering::SeqCst) && Arc::ptr_eq(current, &view),
            "Terminal candidate is no longer current"
        );
        views.forget_writer_candidate(&terminal);
        start_control(&view);
        Ok(())
    }

    async fn attach_view(&self, runtime_id: &str, mode: AttachmentMode) -> Result<Attachment> {
        let generation = self.generation.load(Ordering::SeqCst);
        let runtime_id = Uuid::parse_str(runtime_id)?;
        let directory = self.directory()?;
        let view_id = Uuid::new_v4();
        let writer = mode.is_candidate().then_some((runtime_id, view_id));
        let allocation = self.reserve(generation, writer)?;
        let Response::TerminalOpened { status } = request(
            directory,
            &Request::OpenTerminal {
                runtime_id,
                view_id,
            },
        )
        .await?
        else {
            bail!("Attachment not authorized");
        };
        ensure!(
            status.terminal.target.runtime_id == runtime_id
                && status.terminal.view_id == view_id
                && status.committed.size.supported(),
            "Unexpected terminal attachment"
        );
        mode.check_opened(&status.terminal)?;
        let size = status.committed.size;
        let pair = native_pty_system().openpty(pty_size(size))?;
        ensure!(
            sample_master(pair.master.as_ref())?.size == size,
            "Unexpected attachment PTY grid"
        );
        let tty = pair
            .master
            .tty_name()
            .context("Attachment TTY unavailable")?;
        let fd = pair
            .master
            .as_raw_fd()
            .context("Attachment PTY descriptor unavailable")?;
        set_local_pty_nonblocking(unsafe { BorrowedFd::borrow_raw(fd) })?;
        let mut command = CommandBuilder::new(&self.tmux);
        command.args(["-N", "-S"]);
        command.arg(directory.join("tmux.sock"));
        command.args([
            "attach-session",
            "-f",
            "ignore-size",
            "-E",
            "-t",
            &status.terminal.target.session,
        ]);
        command.env("TERM", "xterm-256color");
        command.env_remove("TMUX");
        let reader = pair.master.try_clone_reader()?;
        let slot = Arc::downgrade(&allocation);
        let child = AttachmentChild::new(pair.slave.spawn_command(command)?, allocation);
        let client_pid = child.owned.as_ref().unwrap().0.process_id().unwrap_or(0);
        drop(pair.slave);
        let (sender, receiver) = mpsc::channel(OUTPUT_SLOTS);
        let mut view = View {
            terminal: status.terminal.clone(),
            directory: directory.clone(),
            tmux: self.tmux.clone(),
            client_pid,
            tty,
            control: Arc::new(tokio::sync::Mutex::new(Control { lease: None })),
            resize: tokio::sync::Mutex::new(()),
            master: Mutex::new(Some(pair.master)),
            output: tokio::sync::Mutex::new(Output {
                receiver,
                refresh_at: Instant::now(),
                status: status.clone(),
                applied_revision: status.committed.revision,
                pending: None,
                renderer_id: None,
                drain_started: None,
                writable: false,
            }),
            drain_revision: AtomicU64::new(0),
            reader_state: Arc::new(AtomicU64::new(RESUME)),
            views: Arc::downgrade(&self.views),
            allocation: slot,
            retirement: watch::channel(None).0,
            reading: AtomicBool::new(false),
            handle: tokio::runtime::Handle::current(),
            #[cfg(test)]
            cleanup_confirmations: AtomicU64::new(0),
        };
        let status = prove_attachment(&view, &status).await?;
        let binding = mode.prepare_control(&view, &status).await?;
        let output = view.output.get_mut();
        output.status = status.clone();
        output.refresh_at = Instant::now() + STATUS_REFRESH_INTERVAL;
        let view = Arc::new(view);
        self.start_attachment(generation, view.clone(), child, reader, sender)?;
        view.trace_pressure("attached");
        #[cfg(test)]
        {
            let gate = self.startup_gate.lock().unwrap().take();
            if let Some(gate) = gate {
                gate.park(view_id).await;
            }
        }
        if let Some(binding) = binding {
            let mut views = self.views.lock().unwrap();
            if let Some(candidate) = views.writer_candidates.get_mut(&runtime_id)
                && candidate.view_id == view_id
            {
                candidate.proof = Some((binding, Instant::now() + WRITER_CANDIDATE_TTL));
            }
        } else if matches!(mode, AttachmentMode::Ordinary) {
            start_control(&view);
        }
        Ok(Attachment {
            view_id: view_id.to_string(),
            cols: size.cols,
            rows: size.rows,
            status,
        })
    }

    fn reserve(&self, generation: u64, writer: Option<(Uuid, Uuid)>) -> Result<Arc<Allocation>> {
        let mut views = self.views.lock().unwrap();
        ensure!(
            generation == self.generation.load(Ordering::SeqCst),
            "View context changed"
        );
        if views.allocated.load(Ordering::Relaxed) == VIEW_LIMIT {
            views.reclaim_eof_slot();
        }
        ensure!(
            views.allocated.load(Ordering::Relaxed) < VIEW_LIMIT,
            "At most 8 local views may be attached"
        );
        views.allocated.fetch_add(1, Ordering::Relaxed);
        let allocation = Arc::new(Allocation(views.allocated.clone()));
        if let Some((runtime, view_id)) = writer {
            views
                .writer_candidates
                .retain(|_, candidate| candidate.allocation.strong_count() != 0);
            views.writer_candidates.insert(
                runtime,
                WriterCandidate {
                    view_id,
                    allocation: Arc::downgrade(&allocation),
                    proof: None,
                },
            );
        }
        Ok(allocation)
    }

    fn start_attachment(
        &self,
        generation: u64,
        view: Arc<View>,
        child: AttachmentChild,
        reader: Box<dyn Read + Send>,
        sender: mpsc::Sender<PtyEvent>,
    ) -> Result<()> {
        let mut views = self.views.lock().unwrap();
        ensure!(
            generation == self.generation.load(Ordering::SeqCst),
            "View context changed"
        );
        let producer = view.clone();
        let (start, begin) = std::sync::mpsc::channel();
        let reader = std::thread::Builder::new()
            .spawn(move || {
                if begin.recv().is_ok() {
                    let mut publisher = PtyPublisher::new(sender);
                    read_pty(
                        reader,
                        &producer.reader_state,
                        |event| producer.publish(&mut publisher, event),
                        |reason| producer.retire(reason),
                    );
                }
            })
            .context("Start attachment reader")?;
        views.live.insert(view.terminal.view_id, view.clone());
        let owner = view.clone();
        tokio::task::spawn_blocking(move || supervise(owner, child, Some(reader)));
        let _ = start.send(());
        Ok(())
    }

    fn view(&self, view_id: &str) -> Result<Arc<View>> {
        let view_id = Uuid::parse_str(view_id).context("View detached")?;
        self.views
            .lock()
            .unwrap()
            .live
            .get(&view_id)
            .cloned()
            .context("View detached")
    }

    fn command_view(&self, view_id: &str, command: TerminalCommand) -> Result<Arc<View>> {
        let id = Uuid::parse_str(view_id).context("View detached")?;
        self.views
            .lock()
            .unwrap()
            .live
            .get(&id)
            .cloned()
            .ok_or_else(|| detached(id, command))
    }

    pub fn attachment_pid(&self, view_id: &str) -> Result<u32> {
        Ok(self.view(view_id)?.client_pid)
    }

    pub async fn status(&self, view_id: &str) -> Result<TerminalStatus> {
        let view = self.view(view_id)?;
        ensure!(!view.closed(), "View detached");
        let status = terminal_status(&view).await;
        if status.is_err() {
            view.retire(Retirement::Failed);
            return status;
        }
        ensure!(!view.closed(), "View detached");
        status
    }

    pub async fn read(&self, view_id: &str) -> Result<Chunk> {
        self.read_with_clock(view_id, Instant::now).await
    }

    async fn read_with_clock(&self, view_id: &str, now: impl Fn() -> Instant) -> Result<Chunk> {
        let id = Uuid::parse_str(view_id).context("View detached")?;
        let view = {
            let mut views = self.views.lock().unwrap();
            let at = Instant::now();
            views.prune(at);
            views.expire_eof(id, at);
            let Some(view) = views.readable_view(&id) else {
                let index = views
                    .notices
                    .iter()
                    .position(|notice| notice.view == id)
                    .ok_or_else(|| detached(id, TerminalCommand::TerminalRead))?;
                return Ok(Chunk::closed(views.notices.remove(index).unwrap().closure));
            };
            ensure!(
                !view.reading.swap(true, Ordering::Relaxed),
                "Terminal read already in progress"
            );
            view.clone()
        };
        let pending = PendingRead {
            view: &view,
            views: &self.views,
            finished: false,
        };
        let result = {
            let Ok(mut output) = view.output.try_lock() else {
                return pending.finish(Err(anyhow::anyhow!(
                    "Terminal boundary already in progress"
                )));
            };
            tokio::select! {
                biased;
                _ = view.discarded() => Err(detached(id, TerminalCommand::TerminalRead)),
                result = read_chunk(&view, &mut output, now) => result,
            }
        };
        if let Err(error) = &result {
            let reason = if error.to_string() == DRAIN_TIMEOUT_ERROR {
                Retirement::Typed(TerminalClosure::GeometryTimeout)
            } else {
                Retirement::Failed
            };
            view.retire(reason);
        }
        pending.finish(result)
    }

    pub async fn resize(
        &self,
        view_id: &str,
        size: TerminalSize,
        epoch: ResizeEpoch,
    ) -> Result<ResizeResult> {
        ensure!(
            size.supported(),
            "Terminal proposal outside supported range"
        );
        let view = self.view(view_id)?;
        let _pending = view
            .resize
            .try_lock()
            .context("Terminal resize already in progress")?;
        let (token, sequence) = {
            let control = view.control.lock().await;
            ensure!(!view.closed(), "View detached");
            let lease = control.lease.as_ref().context("Керування ще не отримано")?;
            ensure!(
                lease.epoch == epoch && lease.writable(Instant::now()),
                "Resize authority changed"
            );
            (lease.token, lease.resize_sequence)
        };
        let reply = view
            .agent_request(&Request::ResizeTerminal {
                terminal: view.terminal.clone(),
                token,
                sequence,
                size,
            })
            .await;
        let mut control = view.control.lock().await;
        ensure!(!view.closed(), "View detached");
        let pending_paste = control
            .lease
            .as_ref()
            .filter(|lease| lease.token == token)
            .context("Resize authority changed")?
            .pending_paste
            .is_some();
        match reply {
            Ok(Response::Resize {
                resize: ResizeResult::Committed { admission, status },
            }) if status.terminal == view.terminal
                && status.epoch == Some(epoch)
                && status.committed.intent == Some(admission)
                && admission.epoch == epoch
                && status.committed.size == size =>
            {
                control.lease.as_mut().unwrap().resize_sequence = sequence
                    .checked_add(1)
                    .context("Resize sequence exhausted")?;
                Ok(ResizeResult::Committed { admission, status })
            }
            Ok(Response::Resize {
                resize: ResizeResult::Uncertain { terminal, intent },
            }) if terminal == view.terminal && intent.epoch == epoch => {
                if pending_paste {
                    control.lease.as_mut().unwrap().deadline = Instant::now();
                } else {
                    control.lease = None;
                }
                view.publish_control(&control);
                Ok(ResizeResult::Uncertain { terminal, intent })
            }
            Ok(Response::Resize {
                resize: ResizeResult::Rejected { terminal, intent },
            }) if terminal == view.terminal && intent.epoch == epoch => {
                if pending_paste {
                    control.lease.as_mut().unwrap().deadline = Instant::now();
                } else {
                    control.lease = None;
                }
                view.publish_control(&control);
                Ok(ResizeResult::Rejected { terminal, intent })
            }
            Ok(Response::Error { message }) => bail!("{message}"),
            _ => {
                if pending_paste {
                    control.lease.as_mut().unwrap().deadline = Instant::now();
                    view.publish_control(&control);
                    bail!("Результат зміни розміру невідомий");
                }
                control.lease = None;
                view.publish_control(&control);
                drop(control);
                view.release(token).await;
                bail!("Результат зміни розміру невідомий");
            }
        }
    }

    pub async fn acknowledge_grid(
        &self,
        view_id: &str,
        acknowledgement: GridAcknowledgement,
    ) -> Result<GeometryEvent> {
        self.acknowledge_grid_with_clock(view_id, acknowledgement, Instant::now)
            .await
    }

    async fn acknowledge_grid_with_clock(
        &self,
        view_id: &str,
        acknowledgement: GridAcknowledgement,
        now: impl Fn() -> Instant,
    ) -> Result<GeometryEvent> {
        let command = TerminalCommand::TerminalAcknowledgeGrid;
        let view = self.command_view(view_id, command)?;
        let detached = || detached(view.terminal.view_id, command);
        let boundary_unavailable = || {
            if view.closed() {
                detached()
            } else {
                anyhow::anyhow!("View detached")
            }
        };
        let mut output = view
            .output
            .try_lock()
            .context("Terminal boundary already in progress")?;
        let Some(GridBarrier::Issued(event)) = &output.pending else {
            bail!("No pending geometry boundary");
        };
        ensure!(
            !view.closed()
                && !acknowledgement.renderer_id.is_nil()
                && output
                    .renderer_id
                    .is_none_or(|id| id == acknowledgement.renderer_id)
                && event.status.epoch == acknowledgement.epoch
                && event.status.committed.revision == acknowledgement.revision
                && acknowledgement.size == event.status.committed.size,
            "Terminal renderer acknowledgement is stale or mismatched"
        );
        let redraw = async {
            let remaining =
                drain_budget(output.drain_started, now())?.context(DRAIN_TIMEOUT_ERROR)?;
            tokio::time::timeout(remaining, redraw_view(&view, event))
                .await
                .context(DRAIN_TIMEOUT_ERROR)?
        };
        let result = tokio::select! {
            biased;
            _ = view.retired() => Err(detached()),
            result = redraw => result,
        };
        let (event, successor) = match result {
            Ok(completed) => completed,
            Err(error) => {
                let reason = if error.to_string() == DRAIN_TIMEOUT_ERROR {
                    Retirement::Typed(TerminalClosure::GeometryTimeout)
                } else {
                    Retirement::Failed
                };
                view.retire(reason);
                return Err(error);
            }
        };
        #[cfg(test)]
        {
            let gate = self.grid_completion_gate.lock().unwrap().take();
            if let Some(gate) = gate {
                gate.park(view.terminal.view_id).await;
            }
        }
        let mut views = self.views.lock().unwrap();
        if view.reader_state.load(Ordering::Acquire) != PAUSE {
            return Err(boundary_unavailable());
        }
        if let Err(error) = drain_budget(output.drain_started, now()) {
            view.retire_locked(
                &mut views,
                Retirement::Typed(TerminalClosure::GeometryTimeout),
            );
            return Err(error);
        }
        if successor.is_none() {
            view.reader_state
                .compare_exchange(PAUSE, RESUME, Ordering::AcqRel, Ordering::Acquire)
                .map_err(|_| boundary_unavailable())?;
            output.drain_started = None;
            view.drain_revision.store(0, Ordering::Relaxed);
        }
        output.pending = successor.map(GridBarrier::Successor);
        output.applied_revision = acknowledgement.revision;
        output.renderer_id = Some(acknowledgement.renderer_id);
        Ok(event)
    }

    pub async fn input(
        &self,
        view_id: &str,
        data: Vec<u8>,
        incarnation: &str,
        paste: bool,
    ) -> Result<()> {
        ensure!(!paste, "PasteUnavailable");
        ensure!(
            (1..=INPUT_CHUNK_LIMIT).contains(&data.len()),
            "Input must contain 1..4096 bytes"
        );
        let incarnation = Uuid::parse_str(incarnation).context("Некоректна прив’язка вводу")?;
        let command = TerminalCommand::TerminalInput;
        let view = self.command_view(view_id, command)?;
        let mut control = view.control.lock().await;
        if view.closed() {
            return Err(detached(view.terminal.view_id, command));
        }
        let lease = control
            .lease
            .as_mut()
            .filter(|lease| lease.deadline > Instant::now() && lease.incarnation == incarnation)
            .ok_or_else(|| stale_input(view.terminal.view_id))?;
        ensure!(lease.pending_paste.is_none(), "Керування втрачено");
        let token = lease.token;
        let sequence = lease.input_sequence;
        lease.input_sequence = sequence
            .checked_add(1)
            .context("Input sequence exhausted")?;
        let runtime_id = view.terminal.target.runtime_id;
        let result = view
            .agent_request(&Request::Input {
                runtime_id,
                token,
                sequence,
                data,
                paste: false,
            })
            .await;
        if matches!(result, Ok(Response::Accepted {})) {
            return Ok(());
        }
        if let Ok(Response::ControlRecovering { paste_admission }) = &result {
            view.hold_paste(*paste_admission);
        }
        control.lease = None;
        view.publish_control(&control);
        drop(control);
        view.release(token).await;
        match result {
            Ok(Response::Error { message }) => bail!("{message}"),
            Err(error) => Err(error.context("Input outcome unknown; input was not retried")),
            _ => bail!("Unexpected input response; input was not retried"),
        }
    }

    pub async fn paste(
        &self,
        view_id: &str,
        incarnation: &str,
        operation: u64,
        action: PasteAction,
    ) -> PasteResult {
        let Ok(incarnation) = Uuid::parse_str(incarnation) else {
            return PasteResult::Invalid {};
        };
        if incarnation.is_nil() || !(1..=(1 << 53) - 1).contains(&operation) {
            return PasteResult::Invalid {};
        }
        let Ok(view) = self.view(view_id) else {
            return PasteResult::Stale {};
        };
        let mut control = view.control.lock().await;
        let Some(lease) = control
            .lease
            .as_mut()
            .filter(|lease| lease.incarnation == incarnation && !view.closed())
        else {
            return PasteResult::Stale {};
        };
        if let PasteAction::Ack { receipt } = action {
            let Ok(receipt) = Uuid::parse_str(&receipt) else {
                return PasteResult::Invalid {};
            };
            let Some(pending) = lease.pending_paste.as_mut().filter(|pending| {
                pending.binding.terminal == view.terminal
                    && pending.binding.token == lease.token
                    && pending.binding.epoch == lease.epoch
                    && pending.binding.incarnation == incarnation
                    && pending.binding.operation == operation
                    && pending.receipt == Some(receipt)
            }) else {
                return PasteResult::Stale {};
            };
            let binding = pending.binding.clone();
            pending.receipt = None;
            let result = view
                .agent_request(&Request::PasteAck { binding, receipt })
                .await;
            if let Ok(Response::Paste {
                paste:
                    PasteResult::Acknowledged {
                        writable: host_writable,
                    },
            }) = result
            {
                let writable = host_writable && lease.deadline > Instant::now() && !view.closed();
                lease.pending_paste = None;
                let release_token = if !writable {
                    let token = lease.token;
                    control.lease = None;
                    Some(token)
                } else {
                    None
                };
                view.publish_control(&control);
                drop(control);
                if host_writable && let Some(token) = release_token {
                    view.release(token).await;
                }
                return PasteResult::Acknowledged { writable };
            }
            return Self::hold_unknown_paste(&view, &mut control);
        }
        if !lease.writable(Instant::now()) {
            return PasteResult::Stale {};
        }
        let binding = PasteBinding {
            terminal: view.terminal.clone(),
            token: lease.token,
            epoch: lease.epoch,
            incarnation,
            operation,
            sequence: lease.input_sequence,
        };
        let request_message = match action {
            PasteAction::Stage { offset, data } => Request::PasteStage {
                binding,
                offset,
                data,
            },
            PasteAction::Commit { bytes } => {
                if lease.input_sequence == u64::MAX {
                    return Self::hold_unknown_paste(&view, &mut control);
                }
                lease.pending_paste = Some(NativePaste {
                    binding: binding.clone(),
                    receipt: None,
                });
                Request::PasteCommit { binding, bytes }
            }
            PasteAction::Ack { .. } => unreachable!("handled receipt above"),
        };
        let committing = matches!(&request_message, Request::PasteCommit { .. });
        match view.agent_request(&request_message).await {
            Ok(Response::Paste {
                paste: PasteResult::Committed { receipt },
            }) if committing && !receipt.is_nil() => {
                lease.input_sequence += 1;
                lease.pending_paste.as_mut().unwrap().receipt = Some(receipt);
                view.publish_lease(lease);
                PasteResult::Committed { receipt }
            }
            Ok(Response::Paste {
                paste: PasteResult::Staged {},
            }) if !committing => PasteResult::Staged {},
            Ok(Response::Paste {
                paste:
                    paste @ (PasteResult::Stale {}
                    | PasteResult::Invalid {}
                    | PasteResult::Unavailable {}),
            }) => {
                lease.pending_paste = None;
                view.publish_lease(lease);
                paste
            }
            Ok(Response::Paste {
                paste: PasteResult::Unknown {},
            }) => Self::hold_unknown_paste(&view, &mut control),
            _ if committing => Self::hold_unknown_paste(&view, &mut control),
            _ => {
                let token = control.lease.take().map(|lease| lease.token);
                view.publish_control(&control);
                drop(control);
                if let Some(token) = token {
                    view.release(token).await;
                }
                PasteResult::Unavailable {}
            }
        }
    }

    fn hold_unknown_paste(view: &Arc<View>, control: &mut Control) -> PasteResult {
        view.hold_paste(PasteAdmission::PriorSendUnknown);
        if let Some(lease) = control.lease.take() {
            view.publish_control(control);
            let view = view.clone();
            tokio::spawn(async move { view.release(lease.token).await });
        } else {
            view.publish_control(control);
        }
        PasteResult::Unknown {}
    }
    pub fn detach(&self, view_id: &str) {
        let Ok(id) = Uuid::parse_str(view_id) else {
            return;
        };
        let mut views = self.views.lock().unwrap();
        if let Some(view) = views.readable_view(&id).cloned() {
            let at = view.retire_locked(&mut views, Retirement::Typed(TerminalClosure::Detached));
            drop(views);
            view.trace_pressure_at("detached", None, at);
        }
    }

    pub fn detach_all(&self) {
        let mut views = self.views.lock().unwrap();
        self.generation.fetch_add(1, Ordering::SeqCst);
        views.writer_candidates.clear();
        let live = views
            .live
            .values()
            .chain(views.drains.values().map(|drain| &drain.view))
            .cloned()
            .collect::<Vec<_>>();
        for view in live {
            let _ = view.retire_locked(&mut views, Retirement::Typed(TerminalClosure::Detached));
        }
    }
}
impl Drop for DesktopBridge {
    fn drop(&mut self) {
        self.detach_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        ffi::OsString,
        fs::{self, DirBuilder},
        os::unix::{ffi::OsStringExt, fs::DirBuilderExt},
        path::Path,
    };
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::UnixListener,
    };

    struct Fixture(PathBuf);

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct PasteIpc {
        bridge: Arc<DesktopBridge>,
        frames: tokio::sync::Mutex<mpsc::UnboundedReceiver<PasteFixtureFrame>>,
        target: remote_sessions_host::model::TerminalTarget,
        agent: Uuid,
    }

    impl PasteIpc {
        fn new() -> Self {
            let (sender, frames) = mpsc::unbounded_channel();
            let bridge = Arc::new(DesktopBridge::new(None, "/unused/tmux".into()));
            bridge.views.lock().unwrap().paste_fixture = Some(sender);
            Self {
                bridge,
                frames: tokio::sync::Mutex::new(frames),
                agent: Uuid::new_v4(),
                target: remote_sessions_host::model::TerminalTarget {
                    runtime_id: Uuid::new_v4(),
                    generation: Uuid::new_v4(),
                    session: "$0".into(),
                    window: "@0".into(),
                    pane: "%0".into(),
                    pid: 42,
                    process_start: 120,
                },
            }
        }

        fn attach(&self) -> (Arc<View>, mpsc::Sender<PtyEvent>) {
            use remote_sessions_host::model::CommittedGeometry;
            let terminal = TerminalView {
                target: self.target.clone(),
                agent_epoch: self.agent,
                view_id: Uuid::new_v4(),
            };
            let status = TerminalStatus {
                terminal: terminal.clone(),
                epoch: None,
                provider: None,
                observation: ObservationState::Unavailable,
                committed: CommittedGeometry {
                    size: TerminalSize {
                        cols: 100,
                        rows: 30,
                    },
                    revision: 0,
                    intent: None,
                },
            };
            let (sender, receiver) = mpsc::channel(1);
            let view = Arc::new(View {
                terminal,
                directory: PathBuf::new(),
                tmux: "/unused/tmux".into(),
                client_pid: 12,
                tty: PathBuf::new(),
                control: Arc::new(tokio::sync::Mutex::new(Control { lease: None })),
                resize: tokio::sync::Mutex::new(()),
                master: Mutex::new(None),
                output: tokio::sync::Mutex::new(Output {
                    status,
                    receiver,
                    refresh_at: Instant::now() + Duration::from_secs(60),
                    applied_revision: 0,
                    pending: None,
                    renderer_id: None,
                    drain_started: None,
                    writable: false,
                }),
                drain_revision: AtomicU64::new(0),
                reader_state: Arc::new(AtomicU64::new(RESUME)),
                views: Arc::downgrade(&self.bridge.views),
                allocation: Weak::new(),
                retirement: watch::channel(None).0,
                reading: AtomicBool::new(false),
                handle: tokio::runtime::Handle::current(),
                cleanup_confirmations: AtomicU64::new(0),
            });
            self.bridge
                .views
                .lock()
                .unwrap()
                .live
                .insert(view.terminal.view_id, view.clone());
            (view, sender)
        }

        async fn receive(&self) -> (tokio::sync::oneshot::Sender<String>, Request) {
            let mut frames = self.frames.lock().await;
            let frame = tokio::time::timeout(Duration::from_secs(2), frames.recv())
                .await
                .unwrap()
                .unwrap();
            let request =
                remote_sessions_host::decode_local_fixture(frame.request.as_bytes()).unwrap();
            (frame.reply, request)
        }

        fn reply(peer: tokio::sync::oneshot::Sender<String>, response: Response) {
            peer.send(encode_local_observation(&response).unwrap() + "\n")
                .unwrap();
        }

        fn refresh(view: &Arc<View>) -> tokio::task::JoinHandle<()> {
            let view = view.clone();
            tokio::spawn(async move { refresh_control(&view).await })
        }

        async fn grant(&self, view: &Arc<View>) -> (Uuid, ResizeEpoch, Uuid) {
            let task = Self::refresh(view);
            let (peer, request) = self.receive().await;
            assert!(
                matches!(request, Request::AcquireTerminal { terminal } if terminal == view.terminal)
            );
            let token = Uuid::new_v4();
            let epoch = ResizeEpoch {
                agent: self.agent,
                owner: Uuid::new_v4(),
            };
            Self::reply(peer, Response::TerminalControl { token, epoch });
            task.await.unwrap();
            view.output.lock().await.status.epoch = Some(epoch);
            let snapshot = self
                .bridge
                .read(&view.terminal.view_id.to_string())
                .await
                .unwrap();
            assert!(snapshot.writable);
            (token, epoch, snapshot.paste_incarnation.unwrap())
        }

        async fn lose(&self, view: &Arc<View>, token: Uuid) {
            let task = Self::refresh(view);
            let (peer, request) = self.receive().await;
            assert!(matches!(request, Request::Renew { token: actual, .. } if actual == token));
            Self::reply(
                peer,
                Response::Error {
                    message: "expired".into(),
                },
            );
            self.release(token).await;
            task.await.unwrap();
        }

        async fn release(&self, token: Uuid) {
            let (peer, request) = self.receive().await;
            assert!(matches!(request, Request::Release { token: actual, .. } if actual == token));
            Self::reply(peer, Response::Accepted {});
        }

        fn invoke(
            &self,
            view: &View,
            incarnation: Uuid,
            operation: u64,
            action: PasteAction,
        ) -> tokio::task::JoinHandle<PasteResult> {
            let bridge = self.bridge.clone();
            let view = view.terminal.view_id.to_string();
            tokio::spawn(async move {
                bridge
                    .paste(&view, &incarnation.to_string(), operation, action)
                    .await
            })
        }

        async fn stage(&self, view: &View, incarnation: Uuid, operation: u64) -> PasteBinding {
            self.stage_payload(view, incarnation, operation).await.0
        }

        async fn stage_payload(
            &self,
            view: &View,
            incarnation: Uuid,
            operation: u64,
        ) -> (PasteBinding, Vec<u8>) {
            let task = self.invoke(
                view,
                incarnation,
                operation,
                PasteAction::Stage {
                    offset: 0,
                    data: vec![b'x'],
                },
            );
            let (peer, request) = self.receive().await;
            let Request::PasteStage {
                binding,
                offset: 0,
                data,
            } = request
            else {
                panic!("expected scoped stage request");
            };
            assert_eq!(binding.terminal, view.terminal);
            assert_eq!(
                (binding.incarnation, binding.operation),
                (incarnation, operation)
            );
            assert_eq!(data, b"x");
            Self::reply(
                peer,
                Response::Paste {
                    paste: PasteResult::Staged {},
                },
            );
            assert_eq!(task.await.unwrap(), PasteResult::Staged {});
            (binding, data)
        }

        async fn no_request(&self) {
            assert!(
                tokio::time::timeout(Duration::from_millis(45), async {
                    self.frames.lock().await.recv().await
                })
                .await
                .is_err()
            );
        }

        fn input(
            &self,
            view: &View,
            incarnation: Uuid,
            byte: u8,
        ) -> tokio::task::JoinHandle<Result<()>> {
            let bridge = self.bridge.clone();
            let id = view.terminal.view_id.to_string();
            tokio::spawn(async move {
                bridge
                    .input(&id, vec![byte], &incarnation.to_string(), false)
                    .await
            })
        }

        async fn reply_inventory(&self, agent_pid: u32, runtimes: Vec<Runtime>) {
            let (peer, request) = self.receive().await;
            assert!(matches!(request, Request::Health {}));
            Self::reply(
                peer,
                Response::Health {
                    agent_pid,
                    remote_execution_enabled: false,
                },
            );
            let (peer, request) = self.receive().await;
            assert!(matches!(request, Request::List {}));
            Self::reply(peer, Response::Runtimes { runtimes });
        }
    }

    #[tokio::test]
    async fn w1_terminal_native_detached_trace_identity() {
        let ipc = PasteIpc::new();
        let (view, _output) = ipc.attach();
        let view_id = view.terminal.view_id;
        let id = view_id.to_string();
        ipc.bridge.detach(&id);
        let acknowledgement = GridAcknowledgement {
            renderer_id: Uuid::new_v4(),
            epoch: None,
            revision: 0,
            size: TerminalSize {
                cols: 100,
                rows: 30,
            },
            focused: false,
        };
        let mut identities = std::collections::HashSet::new();
        for (diagnostic, trace) in [
            (None, true),
            (Some(false), true),
            (Some(true), false),
            (Some(true), true),
        ] {
            let before = sample_clock().unwrap();
            let errors = [
                (
                    ipc.bridge
                        .input(&id, vec![b'x'], &Uuid::new_v4().to_string(), false)
                        .await
                        .err()
                        .unwrap(),
                    "terminal_input",
                    "input",
                ),
                (
                    ipc.bridge.read(&id).await.err().unwrap(),
                    "terminal_read",
                    "read",
                ),
                (
                    ipc.bridge
                        .acknowledge_grid(&id, acknowledgement)
                        .await
                        .err()
                        .unwrap(),
                    "terminal_acknowledge_grid",
                    "acknowledge",
                ),
            ];
            let after = sample_clock().unwrap();
            for (error, command, stage) in errors {
                assert_eq!(error.to_string(), "View detached");
                let (value, observation) = terminal_rejection(error, diagnostic, trace);
                let frame = encode_local_observation(&value).unwrap();
                if diagnostic == Some(true) && trace {
                    let TerminalInputError::Rejected(NativeRejection::ViewDetached {
                        view_id: observed_view,
                        rejection_id,
                        event_time_us,
                        ..
                    }) = value
                    else {
                        panic!("expected detached rejection");
                    };
                    assert_eq!(observed_view, view_id);
                    assert!(!rejection_id.is_nil() && identities.insert(rejection_id));
                    assert!((before..=after).contains(&event_time_us));
                    assert_eq!(observation.as_deref(), Some(frame.as_str()));
                    assert_eq!(
                        frame,
                        format!(
                            r#"{{"reason":"view_detached","view_id":"{view_id}","rejection_id":"{rejection_id}","event_time_us":{event_time_us},"command":"{command}","stage":"{stage}"}}"#
                        )
                    );
                } else {
                    assert_eq!(frame, r#""View detached""#);
                    assert_eq!(observation, None);
                }
            }
        }
        for error in [
            anyhow::anyhow!("View detached"),
            anyhow::anyhow!("Unrelated error"),
            Detached(NativeRejection::ViewDetached {
                view_id,
                rejection_id: Uuid::nil(),
                event_time_us: 0,
                command: TerminalCommand::TerminalRead,
                stage: TerminalStage::Read,
            })
            .into(),
            ipc.bridge.read("invalid view").await.err().unwrap(),
        ] {
            let (value, observation) = terminal_rejection(error, Some(true), true);
            assert!(matches!(value, TerminalInputError::Message(_)));
            assert_eq!(observation, None);
        }
    }

    #[tokio::test]
    async fn w1_terminal_native_rejection_metadata() {
        let ipc = PasteIpc::new();
        let (view, _output) = ipc.attach();
        let (token, _, predecessor) = ipc.grant(&view).await;
        ipc.lose(&view, token).await;
        let (token, _, successor) = ipc.grant(&view).await;
        let before = sample_clock().unwrap();
        let stale = ipc
            .input(&view, predecessor, b'a')
            .await
            .unwrap()
            .unwrap_err();
        let after = sample_clock().unwrap();
        assert_eq!(stale.to_string(), "Керування змінилося");
        let rejection = stale.downcast_ref::<StaleInput>().unwrap().0.clone();
        let rejection_frame = encode_local_observation(&rejection).unwrap();
        let NativeRejection::StaleInput {
            view_id,
            rejection_id,
            event_time_us,
        } = rejection
        else {
            panic!("expected stale input rejection");
        };
        assert_eq!(view_id, view.terminal.view_id);
        assert!(!rejection_id.is_nil());
        assert!((before..=after).contains(&event_time_us));
        let (value, observation) = terminal_rejection(stale, None, true);
        assert_eq!(encode_local_observation(&value).unwrap(), rejection_frame);
        assert_eq!(observation, None);
        assert_eq!(
            rejection_frame,
            format!(
                r#"{{"reason":"stale_input","view_id":"{view_id}","rejection_id":"{rejection_id}","event_time_us":{event_time_us}}}"#
            )
        );
        let control = view.control.lock().await;
        let lease = control.lease.as_ref().unwrap();
        assert_eq!(
            (lease.token, lease.incarnation, lease.input_sequence),
            (token, successor, 0)
        );
        drop(control);
        ipc.no_request().await;

        let send = ipc.input(&view, successor, b'b');
        let (peer, request) = ipc.receive().await;
        assert!(
            matches!(request, Request::Input { token: current, sequence: 0, data, paste: false, .. }
            if current == token && data == b"b")
        );
        PasteIpc::reply(peer, Response::Accepted {});
        send.await.unwrap().unwrap();
        ipc.no_request().await;

        for lost in [false, true] {
            let (token, _, incarnation) = if lost {
                ipc.grant(&view).await
            } else {
                (
                    token,
                    view.control.lock().await.lease.as_ref().unwrap().epoch,
                    successor,
                )
            };
            let send = ipc.input(&view, incarnation, b'c');
            let (peer, request) = ipc.receive().await;
            assert!(matches!(request, Request::Input { .. }));
            if lost {
                drop(peer);
            } else {
                PasteIpc::reply(
                    peer,
                    Response::Error {
                        message: "Керування змінилося".into(),
                    },
                );
            }
            ipc.release(token).await;
            let failure = TerminalInputError::from(send.await.unwrap().unwrap_err());
            assert!(matches!(failure, TerminalInputError::Message(message) if
                message == if lost { "Input outcome unknown; input was not retried" } else { "Керування змінилося" }
            ));
        }
        ipc.no_request().await;
    }

    #[tokio::test]
    async fn w1_terminal_observer_restore_control() {
        let ipc = PasteIpc::new();
        let (observer, _output) = ipc.attach();
        let status = observer.output.lock().await.status.clone();
        assert!(
            AttachmentMode::Observer
                .prepare_control(&observer, &status)
                .await
                .unwrap()
                .is_none()
        );
        let observer_id = observer.terminal.view_id.to_string();
        let operation_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let agent_pid = 12;
        let mode = AttachmentMode::Restore(TerminalRestoration {
            previous: observer.terminal.clone(),
            agent_pid,
            session_id,
            operation_id: Some(operation_id),
        });
        let runtime = Runtime {
            session_id,
            runtime_id: ipc.target.runtime_id,
            tmux_name: "fixture".into(),
            pane_id: ipc.target.pane.clone(),
            command: "fixture".into(),
            pane_pid: Some(ipc.target.pid),
            exited: false,
            width: 100,
            height: 30,
            origin: RuntimeOrigin::Managed,
            workspace_id: Some(Uuid::new_v4()),
            operation_id: Some(operation_id),
        };
        let previous_incarnation = Uuid::new_v4();
        let epoch = ResizeEpoch {
            agent: ipc.agent,
            owner: Uuid::new_v4(),
        };
        assert!(
            ipc.input(&observer, previous_incarnation, b'x')
                .await
                .unwrap()
                .is_err()
        );
        assert!(
            ipc.bridge
                .resize(&observer_id, status.committed.size, epoch)
                .await
                .is_err()
        );
        ipc.no_request().await;
        ipc.bridge.detach(&observer_id);

        let (view, _output) = ipc.attach();
        let id = view.terminal.view_id.to_string();
        let proof = view.output.lock().await.status.clone();
        mode.check_opened(&proof.terminal).unwrap();
        for replacement in [
            TerminalView {
                agent_epoch: Uuid::new_v4(),
                ..proof.terminal.clone()
            },
            TerminalView {
                target: remote_sessions_host::model::TerminalTarget {
                    process_start: ipc.target.process_start + 1,
                    ..ipc.target.clone()
                },
                ..proof.terminal.clone()
            },
        ] {
            assert!(mode.check_opened(&replacement).is_err());
            assert!(
                mode.prepare_control(
                    &view,
                    &TerminalStatus {
                        terminal: replacement,
                        ..proof.clone()
                    }
                )
                .await
                .is_err()
            );
        }
        for (pid, rows) in [
            (agent_pid, vec![]),
            (
                agent_pid,
                vec![Runtime {
                    workspace_id: None,
                    operation_id: None,
                    ..runtime.clone()
                }],
            ),
            (agent_pid + 1, vec![runtime.clone()]),
            (
                agent_pid,
                vec![Runtime {
                    pane_id: "%1".into(),
                    ..runtime.clone()
                }],
            ),
        ] {
            let (admission, ()) = tokio::join!(
                mode.prepare_control(&view, &proof),
                ipc.reply_inventory(pid, rows)
            );
            assert!(admission.is_err());
            assert!(
                ipc.input(&view, previous_incarnation, b'x')
                    .await
                    .unwrap()
                    .is_err()
            );
            assert!(
                ipc.bridge
                    .resize(&id, proof.committed.size, epoch)
                    .await
                    .is_err()
            );
            ipc.no_request().await;
        }

        let (admission, ()) = tokio::join!(
            mode.prepare_control(&view, &proof),
            ipc.reply_inventory(agent_pid, vec![runtime])
        );
        assert!(admission.unwrap().is_some());
        start_control(&view);
        let (peer, request) = ipc.receive().await;
        assert!(
            matches!(request, Request::AcquireTerminal { terminal } if terminal == view.terminal)
        );
        PasteIpc::reply(
            peer,
            Response::Error {
                message: "catalog unavailable".into(),
            },
        );
        assert!(
            ipc.input(&view, previous_incarnation, b'x')
                .await
                .unwrap()
                .is_err()
        );
        assert!(
            ipc.bridge
                .resize(&id, proof.committed.size, epoch)
                .await
                .is_err()
        );
        assert!(!ipc.bridge.read(&id).await.unwrap().writable);
        let (peer, request) = ipc.receive().await;
        assert!(
            matches!(request, Request::AcquireTerminal { terminal } if terminal == view.terminal)
        );
        let token = Uuid::new_v4();
        PasteIpc::reply(peer, Response::TerminalControl { token, epoch });
        assert!(view.control.lock().await.lease.is_some());
        view.output.lock().await.status.epoch = Some(epoch);
        let current = ipc
            .bridge
            .read(&id)
            .await
            .unwrap()
            .paste_incarnation
            .unwrap();
        assert_ne!(current, previous_incarnation);
        assert!(
            ipc.input(&view, previous_incarnation, b'x')
                .await
                .unwrap()
                .is_err()
        );
        let send = ipc.input(&view, current, b'n');
        let (peer, request) = ipc.receive().await;
        assert!(
            matches!(request, Request::Input { token: current, sequence: 0, data, paste: false, .. }
            if current == token && data == b"n")
        );
        PasteIpc::reply(peer, Response::Accepted {});
        send.await.unwrap().unwrap();
        ipc.bridge.detach(&id);
        ipc.no_request().await;
    }

    #[tokio::test]
    async fn w1_terminal_paste_lease_incarnation() {
        let ipc = PasteIpc::new();
        let (view, _output) = ipc.attach();
        let id = view.terminal.view_id.to_string();
        let (first, _, old) = ipc.grant(&view).await;
        let task = PasteIpc::refresh(&view);
        let (peer, request) = ipc.receive().await;
        assert!(matches!(request, Request::Renew { token, .. } if token == first));
        PasteIpc::reply(peer, Response::Accepted {});
        task.await.unwrap();
        assert_eq!(
            ipc.bridge.read(&id).await.unwrap().paste_incarnation,
            Some(old)
        );

        ipc.lose(&view, first).await;
        let (second, _, successor) = ipc.grant(&view).await;
        assert_ne!(old, successor);
        for action in [
            PasteAction::Stage {
                offset: 0,
                data: vec![1],
            },
            PasteAction::Commit { bytes: 1 },
            PasteAction::Ack {
                receipt: Uuid::new_v4().to_string(),
            },
        ] {
            assert_eq!(
                ipc.invoke(&view, old, 1, action).await.unwrap(),
                PasteResult::Stale {}
            );
        }
        ipc.no_request().await;

        let staged = ipc.stage(&view, successor, 2).await;
        assert_eq!(staged.token, second);
        ipc.lose(&view, second).await;
        let (third, epoch, current) = ipc.grant(&view).await;
        assert_ne!(successor, current);
        assert_eq!(
            ipc.invoke(
                &view,
                successor,
                2,
                PasteAction::Stage {
                    offset: 1,
                    data: vec![2]
                }
            )
            .await
            .unwrap(),
            PasteResult::Stale {}
        );
        assert_eq!(
            ipc.invoke(&view, successor, 2, PasteAction::Commit { bytes: 1 })
                .await
                .unwrap(),
            PasteResult::Stale {}
        );
        ipc.no_request().await;

        let staged = ipc.stage(&view, current, 3).await;
        assert_eq!((staged.token, staged.epoch), (third, epoch));
        let task = ipc.invoke(&view, current, 3, PasteAction::Commit { bytes: 1 });
        let (peer, request) = ipc.receive().await;
        assert!(matches!(request, Request::PasteCommit { binding, bytes: 1 } if binding == staged));
        let receipt = Uuid::new_v4();
        PasteIpc::reply(
            peer,
            Response::Paste {
                paste: PasteResult::Committed { receipt },
            },
        );
        assert_eq!(task.await.unwrap(), PasteResult::Committed { receipt });
        let mut control = view.control.lock().await;
        control.lease.as_mut().unwrap().deadline = Instant::now();
        view.publish_control(&control);
        drop(control);
        let completion = ipc.bridge.read(&id).await.unwrap();
        assert!(!completion.writable && completion.paste_incarnation.is_none());
        let completion = completion.paste_completion.unwrap();
        assert_eq!((completion.incarnation, completion.operation), (current, 3));
        assert!(
            ipc.bridge
                .input(&id, vec![b'x'], &current.to_string(), false)
                .await
                .is_err()
        );
        for (incarnation, operation, key) in [
            (old, 3, receipt),
            (current, 2, receipt),
            (current, 3, Uuid::new_v4()),
        ] {
            assert_eq!(
                ipc.invoke(
                    &view,
                    incarnation,
                    operation,
                    PasteAction::Ack {
                        receipt: key.to_string()
                    }
                )
                .await
                .unwrap(),
                PasteResult::Stale {}
            );
        }
        ipc.no_request().await;
        let task = ipc.invoke(
            &view,
            current,
            3,
            PasteAction::Ack {
                receipt: receipt.to_string(),
            },
        );
        let (peer, request) = ipc.receive().await;
        assert!(
            matches!(request, Request::PasteAck { binding, receipt: key } if binding == staged && key == receipt)
        );
        PasteIpc::reply(
            peer,
            Response::Paste {
                paste: PasteResult::Acknowledged { writable: false },
            },
        );
        assert_eq!(
            task.await.unwrap(),
            PasteResult::Acknowledged { writable: false }
        );
        assert!(view.control.lock().await.lease.is_none());
    }

    #[tokio::test]
    async fn w1_terminal_resize_reply_advances_sequence_while_paste_receipt_is_pending() {
        use remote_sessions_host::model::{CommittedGeometry, ResizeIntent};

        let ipc = PasteIpc::new();
        let (view, output) = ipc.attach();
        let id = view.terminal.view_id.to_string();
        let (token, epoch, incarnation) = ipc.grant(&view).await;
        let first_size = TerminalSize {
            cols: 111,
            rows: 34,
        };
        let bridge = ipc.bridge.clone();
        let resize_view = id.clone();
        let first_resize =
            tokio::spawn(async move { bridge.resize(&resize_view, first_size, epoch).await });
        let (resize_reply, request) = ipc.receive().await;
        let Request::ResizeTerminal {
            terminal,
            token: actual_token,
            sequence,
            size,
        } = request
        else {
            panic!("expected the first resize request");
        };
        assert_eq!(terminal, view.terminal);
        assert_eq!((actual_token, sequence, size), (token, 0, first_size));

        let staged = ipc.stage(&view, incarnation, 1).await;
        let commit = ipc.invoke(&view, incarnation, 1, PasteAction::Commit { bytes: 1 });
        let (commit_reply, request) = ipc.receive().await;
        assert!(matches!(request, Request::PasteCommit { binding, bytes: 1 } if binding == staged));
        output
            .send(PtyEvent::Bytes(Arc::new(Mutex::new(
                b"pending commit".to_vec(),
            ))))
            .await
            .unwrap();
        let pending = ipc.bridge.read(&id).await.unwrap();
        assert!(pending.writable);
        assert_eq!(pending.paste_incarnation, Some(incarnation));
        assert!(pending.paste_completion.is_none());
        let receipt = Uuid::new_v4();
        PasteIpc::reply(
            commit_reply,
            Response::Paste {
                paste: PasteResult::Committed { receipt },
            },
        );
        assert_eq!(commit.await.unwrap(), PasteResult::Committed { receipt });
        output
            .send(PtyEvent::Bytes(Arc::new(Mutex::new(b"committed".to_vec()))))
            .await
            .unwrap();
        let committed = ipc.bridge.read(&id).await.unwrap();
        assert!(!committed.writable);
        assert!(committed.paste_incarnation.is_none());
        let completion = committed.paste_completion.unwrap();
        assert_eq!(
            (completion.incarnation, completion.operation),
            (incarnation, 1)
        );

        let first_intent = ResizeIntent {
            id: Uuid::new_v4(),
            epoch,
        };
        let first_status = {
            let mut status = view.output.lock().await.status.clone();
            status.committed = CommittedGeometry {
                size: first_size,
                revision: 1,
                intent: Some(first_intent),
            };
            status
        };
        PasteIpc::reply(
            resize_reply,
            Response::Resize {
                resize: ResizeResult::Committed {
                    admission: first_intent,
                    status: first_status,
                },
            },
        );
        assert!(matches!(
            first_resize.await.unwrap().unwrap(),
            ResizeResult::Committed { admission, .. } if admission == first_intent
        ));
        {
            let control = view.control.lock().await;
            let lease = control.lease.as_ref().unwrap();
            assert_eq!(lease.resize_sequence, 1);
            assert_eq!(lease.pending_paste.as_ref().unwrap().receipt, Some(receipt));
        }

        let acknowledge = ipc.invoke(
            &view,
            incarnation,
            1,
            PasteAction::Ack {
                receipt: receipt.to_string(),
            },
        );
        let (ack_reply, request) = ipc.receive().await;
        assert!(
            matches!(request, Request::PasteAck { binding, receipt: actual } if binding == staged && actual == receipt)
        );
        PasteIpc::reply(
            ack_reply,
            Response::Paste {
                paste: PasteResult::Acknowledged { writable: true },
            },
        );
        assert_eq!(
            acknowledge.await.unwrap(),
            PasteResult::Acknowledged { writable: true }
        );

        let second_size = TerminalSize {
            cols: 119,
            rows: 38,
        };
        let bridge = ipc.bridge.clone();
        let resize_view = id;
        let second_resize =
            tokio::spawn(async move { bridge.resize(&resize_view, second_size, epoch).await });
        let (second_reply, request) = ipc.receive().await;
        let Request::ResizeTerminal { sequence, size, .. } = request else {
            panic!("expected the follow-up resize request");
        };
        assert_eq!((sequence, size), (1, second_size));
        let second_intent = ResizeIntent {
            id: Uuid::new_v4(),
            epoch,
        };
        let second_status = {
            let mut status = view.output.lock().await.status.clone();
            status.committed = CommittedGeometry {
                size: second_size,
                revision: 2,
                intent: Some(second_intent),
            };
            status
        };
        PasteIpc::reply(
            second_reply,
            Response::Resize {
                resize: ResizeResult::Committed {
                    admission: second_intent,
                    status: second_status,
                },
            },
        );
        assert!(matches!(
            second_resize.await.unwrap().unwrap(),
            ResizeResult::Committed { admission, .. } if admission == second_intent
        ));
    }

    #[tokio::test]
    async fn w1_terminal_resize_uncertainty_preserves_pending_paste_receipt() {
        let ipc = PasteIpc::new();
        let (view, _output) = ipc.attach();
        let id = view.terminal.view_id.to_string();
        let (token, epoch, incarnation) = ipc.grant(&view).await;
        let bridge = ipc.bridge.clone();
        let resize_view = id.clone();
        let size = TerminalSize {
            cols: 113,
            rows: 35,
        };
        let resize = tokio::spawn(async move { bridge.resize(&resize_view, size, epoch).await });
        let (resize_reply, request) = ipc.receive().await;
        assert!(
            matches!(request, Request::ResizeTerminal { token: actual, sequence: 0, .. } if actual == token)
        );

        let staged = ipc.stage(&view, incarnation, 1).await;
        let commit = ipc.invoke(&view, incarnation, 1, PasteAction::Commit { bytes: 1 });
        let (commit_reply, request) = ipc.receive().await;
        assert!(matches!(request, Request::PasteCommit { binding, bytes: 1 } if binding == staged));
        let receipt = Uuid::new_v4();
        PasteIpc::reply(
            commit_reply,
            Response::Paste {
                paste: PasteResult::Committed { receipt },
            },
        );
        assert_eq!(commit.await.unwrap(), PasteResult::Committed { receipt });

        let intent = remote_sessions_host::model::ResizeIntent {
            id: Uuid::new_v4(),
            epoch,
        };
        PasteIpc::reply(
            resize_reply,
            Response::Resize {
                resize: ResizeResult::Uncertain {
                    terminal: view.terminal.clone(),
                    intent,
                },
            },
        );
        assert!(matches!(
            resize.await.unwrap().unwrap(),
            ResizeResult::Uncertain { terminal, intent: actual }
                if terminal == view.terminal && actual == intent
        ));
        {
            let control = view.control.lock().await;
            let lease = control.lease.as_ref().unwrap();
            assert!(lease.deadline <= Instant::now());
            assert_eq!(lease.pending_paste.as_ref().unwrap().receipt, Some(receipt));
        }
        let completion = tokio::time::timeout(Duration::from_secs(1), ipc.bridge.read(&id))
            .await
            .unwrap()
            .unwrap();
        let completion = completion.paste_completion.unwrap();
        assert_eq!(
            (completion.incarnation, completion.operation),
            (incarnation, 1)
        );

        let acknowledge = ipc.invoke(
            &view,
            incarnation,
            1,
            PasteAction::Ack {
                receipt: receipt.to_string(),
            },
        );
        let (ack_reply, request) = ipc.receive().await;
        assert!(
            matches!(request, Request::PasteAck { binding, receipt: actual } if binding == staged && actual == receipt)
        );
        PasteIpc::reply(
            ack_reply,
            Response::Paste {
                paste: PasteResult::Acknowledged { writable: false },
            },
        );
        assert_eq!(
            acknowledge.await.unwrap(),
            PasteResult::Acknowledged { writable: false }
        );
        assert!(view.control.lock().await.lease.is_none());
    }

    #[tokio::test]
    async fn w1_terminal_output_progresses_while_control_ipc_is_pending() {
        let ipc = PasteIpc::new();
        let (view, output) = ipc.attach();
        let id = view.terminal.view_id.to_string();
        let (_, _, incarnation) = ipc.grant(&view).await;
        let stage = ipc.invoke(
            &view,
            incarnation,
            1,
            PasteAction::Stage {
                offset: 0,
                data: vec![b'x'],
            },
        );
        let (stage_reply, request) = ipc.receive().await;
        assert!(matches!(request, Request::PasteStage { .. }));
        output
            .send(PtyEvent::Bytes(Arc::new(Mutex::new(
                b"live output".to_vec(),
            ))))
            .await
            .unwrap();

        let chunk = tokio::time::timeout(Duration::from_millis(100), ipc.bridge.read(&id))
            .await
            .expect("terminal read must not wait on control IPC")
            .unwrap();
        assert_eq!(chunk.data, b"live output");
        assert!(chunk.writable);
        assert_eq!(chunk.paste_incarnation, Some(incarnation));
        assert!(chunk.paste_completion.is_none());

        PasteIpc::reply(
            stage_reply,
            Response::Paste {
                paste: PasteResult::Staged {},
            },
        );
        assert_eq!(stage.await.unwrap(), PasteResult::Staged {});
        let ready = tokio::time::timeout(Duration::from_secs(1), ipc.bridge.read(&id))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ready.paste_incarnation, Some(incarnation));
    }

    #[tokio::test]
    async fn w1_terminal_paste_hold_suppresses_published_lease_authority() {
        let ipc = PasteIpc::new();
        let (view, output) = ipc.attach();
        let id = view.terminal.view_id.to_string();
        let (_, _, incarnation) = ipc.grant(&view).await;
        view.hold_paste(PasteAdmission::PriorSendUnknown);
        output
            .send(PtyEvent::Bytes(Arc::new(Mutex::new(b"held".to_vec()))))
            .await
            .unwrap();

        let chunk = ipc.bridge.read(&id).await.unwrap();
        assert!(!chunk.writable);
        assert!(chunk.paste_incarnation.is_none());
        assert!(chunk.paste_completion.is_none());
        assert_eq!(
            chunk.paste_admission,
            Some(PasteAdmission::PriorSendUnknown)
        );
        assert_eq!(
            view.control
                .lock()
                .await
                .lease
                .as_ref()
                .unwrap()
                .incarnation,
            incarnation
        );
    }

    #[tokio::test]
    async fn w1_terminal_paste_lost_reply() {
        let mut observed_effects = Vec::new();
        for apply in [false, true] {
            let ipc = PasteIpc::new();
            let (view, _output) = ipc.attach();
            let id = view.terminal.view_id.to_string();
            let (token, _, incarnation) = ipc.grant(&view).await;
            assert_eq!(
                ipc.bridge
                    .input(&id, vec![1], &incarnation.to_string(), true)
                    .await
                    .unwrap_err()
                    .to_string(),
                "PasteUnavailable"
            );
            let (staged, payload) = ipc.stage_payload(&view, incarnation, 1).await;
            assert_eq!(staged.sequence, 0);
            let task = ipc.invoke(&view, incarnation, 1, PasteAction::Commit { bytes: 1 });
            let (peer, request) = ipc.receive().await;
            assert!(
                matches!(request, Request::PasteCommit { binding, bytes: 1 } if binding == staged)
            );
            let mut delivered_bytes = Vec::new();
            let receipt = apply.then(|| {
                delivered_bytes.push(payload);
                Uuid::new_v4()
            });
            drop(peer);
            assert_eq!(task.await.unwrap(), PasteResult::Unknown {});
            ipc.release(token).await;
            let chunk = ipc.bridge.read(&id).await.unwrap();
            assert_eq!(
                chunk.paste_admission,
                Some(PasteAdmission::PriorSendUnknown)
            );
            assert!(
                !chunk.writable
                    && chunk.paste_incarnation.is_none()
                    && chunk.paste_completion.is_none()
            );
            assert!(
                ipc.bridge
                    .input(&id, vec![1], &incarnation.to_string(), false)
                    .await
                    .is_err()
            );
            assert_eq!(
                ipc.invoke(&view, incarnation, 1, PasteAction::Commit { bytes: 1 })
                    .await
                    .unwrap(),
                PasteResult::Stale {}
            );
            let unobserved_receipt = receipt.unwrap_or_else(Uuid::new_v4);
            assert_eq!(
                ipc.invoke(
                    &view,
                    incarnation,
                    1,
                    PasteAction::Ack {
                        receipt: unobserved_receipt.to_string()
                    }
                )
                .await
                .unwrap(),
                PasteResult::Stale {}
            );
            ipc.no_request().await;

            ipc.bridge.detach(&id);
            let (fresh, _fresh_output) = ipc.attach();
            let refresh = PasteIpc::refresh(&fresh);
            let (peer, request) = ipc.receive().await;
            assert!(
                matches!(request, Request::AcquireTerminal { terminal } if terminal == fresh.terminal)
            );
            PasteIpc::reply(
                peer,
                Response::ControlRecovering {
                    paste_admission: PasteAdmission::PriorSendUnknown,
                },
            );
            refresh.await.unwrap();
            let fresh_id = fresh.terminal.view_id.to_string();
            assert!(!ipc.bridge.read(&fresh_id).await.unwrap().writable);
            assert_eq!(
                ipc.invoke(
                    &fresh,
                    incarnation,
                    1,
                    PasteAction::Stage {
                        offset: 1,
                        data: vec![3]
                    }
                )
                .await
                .unwrap(),
                PasteResult::Stale {}
            );
            ipc.no_request().await;
            observed_effects.push(delivered_bytes);
        }
        assert!(observed_effects[0].is_empty());
        assert_eq!(observed_effects[1], [b"x".to_vec()]);

        let ipc = PasteIpc::new();
        let (view, _output) = ipc.attach();
        let (_, _, incarnation) = ipc.grant(&view).await;
        let staged = ipc.stage(&view, incarnation, 1).await;
        let sent = ipc.invoke(&view, incarnation, 1, PasteAction::Commit { bytes: 1 });
        let (peer, request) = ipc.receive().await;
        assert!(matches!(request, Request::PasteCommit { binding, bytes: 1 } if binding == staged));
        let receipt = Uuid::new_v4();
        PasteIpc::reply(
            peer,
            Response::Paste {
                paste: PasteResult::Committed { receipt },
            },
        );
        assert_eq!(sent.await.unwrap(), PasteResult::Committed { receipt });
        let acknowledged = ipc.invoke(
            &view,
            incarnation,
            1,
            PasteAction::Ack {
                receipt: receipt.to_string(),
            },
        );
        let (peer, request) = ipc.receive().await;
        assert!(matches!(
            request, Request::PasteAck { binding, receipt: prior } if binding == staged && prior == receipt
        ));
        PasteIpc::reply(
            peer,
            Response::Paste {
                paste: PasteResult::Acknowledged { writable: true },
            },
        );
        assert_eq!(
            acknowledged.await.unwrap(),
            PasteResult::Acknowledged { writable: true }
        );
        ipc.no_request().await;
    }

    #[tokio::test]
    async fn w1_readiness_cancel_and_non_utf8_never_contact_agent() {
        let bridge = DesktopBridge::new(None, "/unused/tmux".into());
        assert_eq!(
            bridge.pick_workspace(None).await.unwrap(),
            WorkspaceSelection::Cancelled {}
        );
        let non_utf8 = Path::new("/tmp").join(OsString::from_vec(vec![0xff]));
        assert_eq!(
            bridge.pick_workspace(Some(non_utf8)).await.unwrap(),
            WorkspaceSelection::Blocked {
                issue: ProductIssue::UnsupportedPath
            }
        );
    }

    #[tokio::test]
    async fn w1_readiness_bridge_uses_typed_ipc_and_omits_locator_from_reply() {
        let root = Path::new("/tmp")
            .canonicalize()
            .unwrap()
            .join(format!("rs-{}", Uuid::new_v4().simple()));
        DirBuilder::new().mode(0o700).create(&root).unwrap();
        let fixture = Fixture(root);
        let locator = fixture.0.join("Проєкт з пробілами");
        fs::create_dir(&locator).unwrap();
        let listener = UnixListener::bind(fixture.0.join("agent.sock")).unwrap();
        let id = Uuid::new_v4();
        let expected = format!(
            "{{\"product\":{{\"op\":\"register_workspace\",\"locator\":\"{}\"}}}}\n",
            locator.display()
        );
        let readiness = format!(
            "{{\"product\":{{\"op\":\"workspace_readiness\",\"workspace_id\":\"{id}\",\"profile\":\"claude_code\"}}}}\n"
        );
        let registered = format!(
            "{{\"product\":{{\"result\":\"workspace_registered\",\"workspace\":{{\"workspace_id\":\"{id}\",\"label\":\"Проєкт з пробілами\"}}}}}}\n"
        );
        let server = async move {
            for (expected, response) in [
                (expected, registered),
                (readiness, "{\"product\":{\"result\":\"blocked\",\"issue\":\"interpreter_unavailable\"}}\n".into()),
            ] {
                let (mut client, _) = listener.accept().await.unwrap();
                let mut line = String::new();
                BufReader::new(&mut client).read_line(&mut line).await.unwrap();
                assert_eq!(line, expected);
                client.write_all(response.as_bytes()).await.unwrap();
            }
        };
        let browser = async {
            let bridge = DesktopBridge::new(Some(fixture.0.clone()), "/unused/tmux".into());
            let WorkspaceSelection::Selected { workspace } =
                bridge.pick_workspace(Some(locator)).await.unwrap()
            else {
                panic!("expected selected project")
            };
            assert_eq!(workspace.workspace_id, id);
            assert_eq!(workspace.label, "Проєкт з пробілами");
            assert_eq!(
                bridge
                    .workspace_readiness(
                        &workspace.workspace_id.to_string(),
                        ProviderProfile::ClaudeCode
                    )
                    .await
                    .unwrap(),
                WorkspaceReadiness::Blocked {
                    issue: ProductIssue::InterpreterUnavailable
                }
            );
        };
        tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(server, browser);
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn w1_resize_w1_terminal_private_ipc_admission_and_ordered_independent_grids() {
        ordered_independent_grids(false).await;
    }

    #[tokio::test]
    async fn w1_resize_w1_terminal_eof_during_pending_geometry() {
        ordered_independent_grids(true).await;
    }

    async fn ordered_independent_grids(eof_while_pending: bool) {
        use remote_sessions_host::{
            decode_local_fixture, encode_local_observation,
            model::{CommittedGeometry, ProviderSample, ResizeIntent, TerminalTarget},
        };
        use std::os::unix::fs::PermissionsExt;

        let root = Path::new("/tmp")
            .canonicalize()
            .unwrap()
            .join(format!("rs-{}", Uuid::new_v4().simple()));
        DirBuilder::new().mode(0o700).create(&root).unwrap();
        let fixture = Fixture(root);
        let tmux = fixture.0.join("tmux");
        let epoch = ResizeEpoch {
            agent: Uuid::new_v4(),
            owner: Uuid::new_v4(),
        };
        let generation = Uuid::new_v4();
        let initial_size = TerminalSize {
            cols: 100,
            rows: 30,
        };
        let desired = TerminalSize {
            cols: 117,
            rows: 39,
        };
        let runtime_id = Uuid::new_v4();
        let token = Uuid::new_v4();
        let intent = ResizeIntent {
            id: Uuid::new_v4(),
            epoch,
        };
        let listener = UnixListener::bind(fixture.0.join("agent.sock")).unwrap();
        let program = r#"#!/bin/sh
set -eu
root=$(dirname "$0")
case "$4" in
  attach-session)
    printf '%s\n%s\n' "$$" "$(tty)" > "$root/client"
    printf 'initial stream\033['
    while [ ! -f "$root/suffix" ]; do /bin/sleep 0.01; done
    printf '2H'
    while [ ! -f "$root/redraw" ]; do
      if [ -f "$root/end-while-paused" ]; then
        printf 'unacknowledged grid output'
        : > "$root/final-written"
        exit 0
      fi
      /bin/sleep 0.01
    done
    printf 'redraw stream'
    exec /bin/sleep 30
    ;;
  list-clients)
    { read -r pid; read -r tty; } < "$root/client"
    set -- $(stty size < "$tty")
    printf '%s|%s|$0|@0|%%0|GENERATION|ignore-size|%s|%s\n' "$pid" "$tty" "$2" "$1"
    ;;
  refresh-client)
    { read -r pid; read -r tty; } < "$root/client"
    test "$6" = "$tty"
    printf '%s' redraw > "$root/redraw"
    ;;
  *) exit 1 ;;
esac
"#;
        fs::write(
            &tmux,
            program.replace("GENERATION", &generation.to_string()),
        )
        .unwrap();
        fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();

        let admission_count = Arc::new(AtomicU64::new(0));
        let dispatched = admission_count.clone();
        let provider_reached = Arc::new(AtomicU64::new(0));
        let provider = provider_reached.clone();
        let server = tokio::spawn(async move {
            let mut status = None;
            loop {
                let (mut peer, _) = listener.accept().await.unwrap();
                let mut line = String::new();
                BufReader::new(&mut peer)
                    .read_line(&mut line)
                    .await
                    .unwrap();
                let reply = match decode_local_fixture::<Request>(line.as_bytes()).unwrap() {
                    Request::OpenTerminal {
                        runtime_id: actual,
                        view_id,
                    } => {
                        assert_eq!(actual, runtime_id);
                        let current = TerminalStatus {
                            terminal: TerminalView {
                                target: TerminalTarget {
                                    runtime_id,
                                    generation,
                                    session: "$0".into(),
                                    window: "@0".into(),
                                    pane: "%0".into(),
                                    pid: 123,
                                    process_start: 456,
                                },
                                view_id,
                                agent_epoch: epoch.agent,
                            },
                            epoch: Some(epoch),
                            committed: CommittedGeometry {
                                size: initial_size,
                                revision: 0,
                                intent: None,
                            },
                            provider: Some(ProviderSample {
                                size: initial_size,
                                sampled_at_us: 1,
                            }),
                            observation: ObservationState::Observed,
                        };
                        status = Some(current.clone());
                        Response::TerminalOpened { status: current }
                    }
                    Request::AcquireTerminal { terminal } => {
                        assert_eq!(status.as_ref().unwrap().terminal, terminal);
                        Response::TerminalControl { token, epoch }
                    }
                    Request::ResizeTerminal {
                        terminal,
                        token: actual,
                        sequence,
                        size,
                    } => {
                        let current = status.as_mut().unwrap();
                        assert_eq!(terminal, current.terminal);
                        assert_eq!(actual, token);
                        assert_eq!(sequence, 0);
                        assert_eq!(size, desired);
                        dispatched.fetch_add(1, Ordering::Relaxed);
                        current.committed = CommittedGeometry {
                            size,
                            revision: 1,
                            intent: Some(intent),
                        };
                        current.observation = ObservationState::Pending;
                        Response::Resize {
                            resize: ResizeResult::Committed {
                                admission: intent,
                                status: current.clone(),
                            },
                        }
                    }
                    Request::TerminalStatus { terminal } => {
                        let current = status.as_mut().unwrap();
                        assert_eq!(terminal, current.terminal);
                        if provider.load(Ordering::Relaxed) == 1 {
                            current.provider = Some(ProviderSample {
                                size: desired,
                                sampled_at_us: 2,
                            });
                            current.observation = ObservationState::Observed;
                        }
                        Response::TerminalStatus {
                            status: current.clone(),
                        }
                    }
                    Request::Renew { token: actual, .. }
                    | Request::Release { token: actual, .. } => {
                        assert_eq!(token, actual);
                        Response::Accepted {}
                    }
                    other => panic!("Unexpected request: {other:?}"),
                };
                let line = encode_local_observation(&reply).unwrap() + "\n";
                peer.write_all(line.as_bytes()).await.unwrap();
            }
        });

        let bridge = DesktopBridge::new(Some(fixture.0.clone()), tmux);
        let view = bridge.attach(&runtime_id.to_string()).await.unwrap();
        let key = &view.view_id;
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut output = Vec::new();
            let incarnation = loop {
                let chunk = bridge.read(key).await.unwrap();
                assert!(!chunk.closed);
                output.extend(chunk.data);
                if chunk.writable && output == b"initial stream\x1b[" {
                    break chunk.paste_incarnation.unwrap().to_string();
                }
            };
            assert!(
                bridge
                    .resize(
                        key,
                        TerminalSize {
                            rows: 201,
                            ..desired
                        },
                        epoch
                    )
                    .await
                    .is_err()
            );
            assert_eq!(admission_count.load(Ordering::Relaxed), 0);
            let ResizeResult::Committed { status, .. } =
                bridge.resize(key, desired, epoch).await.unwrap()
            else {
                panic!("Expected a committed request");
            };
            assert_eq!(status.committed.revision, 1);
            assert_eq!(status.provider.unwrap().size, initial_size);
            assert_eq!(admission_count.load(Ordering::Relaxed), 1);

            let live = bridge.view(key).unwrap();
            live.output.lock().await.refresh_at = Instant::now();
            let draining = bridge.read(key).await.unwrap();
            assert_eq!(draining.status.unwrap().committed.revision, 1);
            assert!(draining.geometry.is_none() && draining.data.is_empty());
            assert_eq!(live.reader_state.load(Ordering::Acquire), DRAIN);
            let started = live.output.lock().await.drain_started.unwrap();
            live.output.lock().await.refresh_at = Instant::now();
            let still_draining = bridge.read(key).await.unwrap();
            assert!(still_draining.status.is_some());
            assert!(still_draining.geometry.is_none() && still_draining.data.is_empty());
            assert_eq!(live.output.lock().await.drain_started, Some(started));
            assert_eq!(
                sample_master(live.master.lock().unwrap().as_deref().unwrap())
                    .unwrap()
                    .size,
                initial_size
            );
            let acknowledgement = GridAcknowledgement {
                renderer_id: Uuid::new_v4(),
                epoch: Some(epoch),
                revision: 1,
                size: desired,
                focused: true,
            };
            assert!(bridge.acknowledge_grid(key, acknowledgement).await.is_err());
            assert!(!fixture.0.join("redraw").exists());
            fs::write(fixture.0.join("suffix"), b"").unwrap();
            let event = loop {
                let chunk = bridge.read(key).await.unwrap();
                assert!(!chunk.closed);
                output.extend(chunk.data);
                if let Some(event) = chunk.geometry {
                    break event;
                }
            };
            assert_eq!(output, b"initial stream\x1b[2H");
            assert_eq!(event.status.terminal, view.status.terminal);
            assert_eq!(event.client.size, desired);
            assert_eq!(event.bridge_pty.size, desired);
            assert_eq!(live.reader_state.load(Ordering::Acquire), PAUSE);
            assert!(!fixture.0.join("redraw").exists());
            for wrong in [
                GridAcknowledgement {
                    revision: 0,
                    ..acknowledgement
                },
                GridAcknowledgement {
                    epoch: None,
                    ..acknowledgement
                },
                GridAcknowledgement {
                    size: initial_size,
                    ..acknowledgement
                },
            ] {
                assert!(bridge.acknowledge_grid(key, wrong).await.is_err());
                assert_eq!(live.reader_state.load(Ordering::Acquire), PAUSE);
            }
            assert!(!fixture.0.join("redraw").exists());
            if eof_while_pending {
                fs::write(fixture.0.join("end-while-paused"), b"").unwrap();
                while !fixture.0.join("final-written").exists() {
                    tokio::time::sleep(CLEANUP_POLL).await;
                }
                {
                    let output = live.output.lock().await;
                    let Some(GridBarrier::Issued(pending)) = &output.pending else {
                        panic!("Expected the issued geometry");
                    };
                    assert_eq!(pending.status, event.status);
                    assert!(output.receiver.is_empty());
                    assert_eq!(output.applied_revision, 0);
                    assert_eq!(live.reader_state.load(Ordering::Acquire), PAUSE);
                }
                live.retire(Retirement::Eof);
                assert!(bridge.acknowledge_grid(key, acknowledgement).await.is_err());
                let closed = bridge.read(key).await.unwrap();
                assert_eq!(closed.closure, Some(TerminalClosure::GeometryTimeout));
                assert!(closed.closed && !closed.writable && closed.data.is_empty());
                assert!(closed.geometry.is_none() && closed.status.is_none());
                assert!(bridge.read(key).await.is_err());
                assert!(
                    bridge
                        .input(key, b"after close".to_vec(), &incarnation, false)
                        .await
                        .is_err()
                );
                assert!(!fixture.0.join("redraw").exists());
                return;
            }
            provider_reached.store(1, Ordering::Relaxed);
            let observed = bridge.acknowledge_grid(key, acknowledgement).await.unwrap();
            assert_eq!(observed.status.provider.unwrap().size, desired);
            assert_eq!(observed.bridge_pty.size, desired);
            assert_eq!(observed.client.size, desired);
            assert_eq!(
                fs::read_to_string(fixture.0.join("redraw")).unwrap(),
                "redraw"
            );
            assert_eq!(live.reader_state.load(Ordering::Acquire), RESUME);
            let mut resumed = Vec::new();
            while resumed.len() < b"redraw stream".len() {
                let chunk = bridge.read(key).await.unwrap();
                assert!(!chunk.closed && chunk.geometry.is_none());
                resumed.extend(chunk.data);
            }
            assert_eq!(resumed, b"redraw stream");
            bridge.detach(key);
            assert!(bridge.resize(key, desired, epoch).await.is_err());
        })
        .await
        .unwrap();
        server.abort();
    }
}
