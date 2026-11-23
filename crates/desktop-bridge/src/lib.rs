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
pub use remote_sessions_host::model::{ProductIssue, ProviderProfile, WorkspaceDisplay};
pub use remote_sessions_host::model::{ResizeEpoch, ResizeResult, TerminalSize, TerminalStatus};
use remote_sessions_host::{
    engine::set_local_pty_nonblocking,
    model::{
        ObservationState, ProductRequest, ProductResponse, Request, Response, Runtime, TerminalView,
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
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

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
#[derive(Serialize)]
pub struct Chunk {
    pub writable: bool,
    pub data: Vec<u8>,
    pub closed: bool,
    pub status: Option<TerminalStatus>,
    pub geometry: Option<GeometryEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closure: Option<TerminalClosure>,
}

impl Chunk {
    fn closed(closure: TerminalClosure) -> Self {
        Self {
            writable: false,
            data: Vec::new(),
            closed: true,
            status: None,
            geometry: None,
            closure: Some(closure),
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
    ReceiverClosed,
    Failed,
}

const VIEW_LIMIT: usize = 8;
const OUTPUT_SLOTS: usize = 16;
const NOTICE_TTL: Duration = Duration::from_secs(120);
const CLEANUP_BUDGET: Duration = Duration::from_secs(2);
const CLEANUP_POLL: Duration = Duration::from_millis(5);

#[derive(Default)]
struct Views {
    live: HashMap<Uuid, Arc<View>>,
    notices: VecDeque<ClosureNotice>,
    allocated: usize,
}

struct ClosureNotice {
    view: Uuid,
    closure: TerminalClosure,
    expires: Instant,
}

impl Views {
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
}

struct Allocation(Arc<Mutex<Views>>);

impl Drop for Allocation {
    fn drop(&mut self) {
        self.0.lock().unwrap().allocated -= 1;
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
}
enum PtyEvent {
    Bytes(Vec<u8>),
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

struct Output {
    receiver: mpsc::Receiver<PtyEvent>,
    status: TerminalStatus,
    refresh_at: Instant,
    applied_revision: u64,
    pending: Option<GeometryEvent>,
    drain_started: Option<Instant>,
    writable: bool,
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
    retirement: watch::Sender<Option<Retirement>>,
    reading: AtomicBool,
    handle: tokio::runtime::Handle,
    #[cfg(test)]
    cleanup_confirmations: AtomicU64,
}
impl View {
    fn closed(&self) -> bool {
        self.reader_state.load(Ordering::Acquire) == CLOSE
    }

    fn publish(&self, sender: &mpsc::Sender<PtyEvent>, event: PtyEvent) -> bool {
        let Some(registry) = self.views.upgrade() else {
            return false;
        };
        let mut views = registry.lock().unwrap();
        if self.closed() {
            return false;
        }
        let Some(reason) = publish(sender, event) else {
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
        views.live.remove(&id)?;
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
            request(
                &self.directory,
                &Request::Release {
                    runtime_id: self.terminal.target.runtime_id,
                    token,
                },
            ),
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
        let _views = self.views.lock().unwrap();
        self.view.reading.store(false, Ordering::Relaxed);
        self.finished = true;
        match *self.view.retirement.borrow() {
            Some(Retirement::Typed(closure)) => Ok(Chunk::closed(closure)),
            Some(_) if result.is_ok() => bail!("View detached"),
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
    mut child: Box<dyn Child + Send + Sync>,
    reader: Option<std::thread::JoinHandle<()>>,
    allocation: Allocation,
) {
    view.handle.block_on(view.retired());
    let deadline = Instant::now() + CLEANUP_BUDGET;
    view.master.lock().unwrap().take();
    let cleanup = view.clone();
    let output_cleanup = view.handle.spawn(async move {
        let mut output = cleanup.output.lock().await;
        output.receiver.close();
        while output.receiver.try_recv().is_ok() {}
        output.pending = None;
        output.drain_started = None;
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
        if !reap {
            match child.try_wait() {
                Ok(Some(_)) => {
                    reap = true;
                    #[cfg(test)]
                    view.cleanup_confirmations.fetch_or(2, Ordering::Release);
                    view.trace_pressure("child_reaped");
                }
                Ok(None) if !signalled => {
                    signalled = true;
                    let _ = child.kill();
                }
                _ => {}
            }
        }
        if reap && reader.is_none() && output_cleanup.is_finished() {
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

fn publish(sender: &mpsc::Sender<PtyEvent>, event: PtyEvent) -> Option<Retirement> {
    match sender.try_send(event) {
        Ok(()) => None,
        Err(mpsc::error::TrySendError::Full(_)) => {
            Some(Retirement::Typed(TerminalClosure::Overflow {
                high_water: sender.max_capacity(),
            }))
        }
        Err(mpsc::error::TrySendError::Closed(_)) => Some(Retirement::ReceiverClosed),
    }
}

fn read_pty(
    mut reader: Box<dyn Read + Send>,
    state: &AtomicU64,
    publish: impl Fn(PtyEvent) -> bool,
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
                    if complete != 0 && !publish(PtyEvent::Bytes(bytes[..complete].to_vec())) {
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
                        && !publish(PtyEvent::Boundary)
                    {
                        return;
                    }
                    std::thread::sleep(CLEANUP_POLL);
                }
                Err(_) => break,
            },
        }
    }
    retire(Retirement::Typed(TerminalClosure::Eof));
}

async fn refresh_control(view: &View) {
    let mut control = view.control.lock().await;
    if view.closed() {
        return;
    }
    let message = match &control.lease {
        Some(lease) => Request::Renew {
            runtime_id: view.terminal.target.runtime_id,
            token: lease.token,
        },
        None => Request::AcquireTerminal {
            terminal: view.terminal.clone(),
        },
    };
    match request(&view.directory, &message).await {
        Ok(Response::TerminalControl { token, epoch }) => {
            {
                let views = view.views.upgrade();
                let _registry = views.as_ref().map(|views| views.lock().unwrap());
                if !view.closed() && epoch.agent == view.terminal.agent_epoch {
                    control.lease = Some(NativeLease {
                        token,
                        epoch,
                        input_sequence: 0,
                        resize_sequence: 0,
                    });
                    return;
                }
            }
            drop(control);
            view.release(token).await;
        }
        Ok(Response::Accepted {}) => {}
        _ => {
            control.lease = None;
        }
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

async fn redraw_view(view: &View, boundary: &GeometryEvent) -> Result<GeometryEvent> {
    let status = terminal_status(view).await?;
    ensure!(
        status.epoch == boundary.status.epoch && status.committed == boundary.status.committed,
        "Terminal geometry boundary was superseded"
    );
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
    view_command(
        view,
        &[
            "refresh-client",
            "-t",
            view.tty.to_str().context("Invalid attachment TTY")?,
        ],
    )
    .await?;
    Ok(GeometryEvent {
        status,
        bridge_pty,
        client,
    })
}

fn expire_drain(registry: &Mutex<Views>, id: Uuid, revision: u64) {
    let mut views = registry.lock().unwrap();
    if let Some(view) = views.live.get(&id).cloned()
        && view.drain_revision.load(Ordering::Relaxed) == revision
    {
        view.retire_locked(
            &mut views,
            Retirement::Typed(TerminalClosure::GeometryTimeout),
        );
    }
}

async fn read_chunk(view: &View, output: &mut Output, now: impl Fn() -> Instant) -> Result<Chunk> {
    ensure!(
        output.pending.is_none(),
        "Terminal grid acknowledgement required"
    );
    let mut chunk = Chunk {
        writable: false,
        data: Vec::new(),
        closed: false,
        status: None,
        geometry: None,
        closure: None,
    };
    let reading_at = now();
    let remaining = drain_budget(output.drain_started, reading_at)?;
    if reading_at >= output.refresh_at {
        let status = terminal_status_during_drain(view, remaining).await;
        drain_budget(output.drain_started, now())?;
        match status {
            Ok(status) if status.committed.revision >= output.status.committed.revision => {
                output.status = status
            }
            Ok(_) => {}
            Err(_) => {
                output.status.provider = None;
                output.status.observation = ObservationState::Unavailable;
            }
        }
        output.refresh_at = now() + Duration::from_millis(150);
        chunk.status = Some(output.status.clone());
    }
    let draining_at = now();
    if output.status.committed.revision > output.applied_revision
        && view
            .reader_state
            .compare_exchange(RESUME, DRAIN, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    {
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
    let poll = Duration::from_millis(150);
    let wait = drain_budget(output.drain_started, draining_at)?
        .unwrap_or(poll)
        .min(poll);
    let event = tokio::time::timeout(wait, output.receiver.recv()).await;
    let remaining = drain_budget(output.drain_started, now())?;
    match event {
        Ok(Some(PtyEvent::Bytes(data))) => chunk.data = data,
        Ok(Some(PtyEvent::Boundary)) => {
            let status = terminal_status_during_drain(view, remaining).await?;
            let remaining =
                drain_budget(output.drain_started, now())?.context(DRAIN_TIMEOUT_ERROR)?;
            ensure!(
                status.committed.revision >= output.status.committed.revision,
                "Terminal geometry revision went backwards"
            );
            let event = tokio::time::timeout(remaining, prepare_grid(view, status))
                .await
                .context(DRAIN_TIMEOUT_ERROR)??;
            drain_budget(output.drain_started, now())?;
            output.status = event.status.clone();
            output.pending = Some(event.clone());
            chunk.status = None;
            chunk.geometry = Some(event);
        }
        Ok(None) => {
            view.retire(Retirement::ReceiverClosed);
        }
        Err(_) => {}
    }
    if let Ok(control) = view.control.try_lock() {
        output.writable = control
            .lease
            .as_ref()
            .is_some_and(|lease| Some(lease.epoch) == output.status.epoch);
    }
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
    startup_gate: Mutex<Option<w1_terminal_tests::StartupGate>>,
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
        let directory = self.directory()?;
        let Response::Health { agent_pid, .. } = request(directory, &Request::Health {}).await?
        else {
            bail!("Agent health unavailable")
        };
        match request(directory, &Request::List {}).await? {
            Response::Runtimes { runtimes } => Ok(Inventory {
                agent_pid,
                runtimes,
            }),
            Response::Error { message } => bail!("{message}"),
            _ => bail!("Unexpected inventory response"),
        }
    }
    pub async fn attach(&self, runtime_id: &str) -> Result<Attachment> {
        self.attach_view(runtime_id, true).await
    }

    pub async fn attach_observer(&self, runtime_id: &str) -> Result<Attachment> {
        self.attach_view(runtime_id, false).await
    }

    async fn attach_view(&self, runtime_id: &str, request_control: bool) -> Result<Attachment> {
        let generation = self.generation.load(Ordering::SeqCst);
        let runtime_id = Uuid::parse_str(runtime_id)?;
        let directory = self.directory()?;
        let view_id = Uuid::new_v4();
        let allocation = self.reserve(generation)?;
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
        let child = pair.slave.spawn_command(command)?;
        let client_pid = child.process_id().unwrap_or(0);
        drop(pair.slave);
        let (sender, receiver) = mpsc::channel(OUTPUT_SLOTS);
        let view = Arc::new(View {
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
                drain_started: None,
                writable: false,
            }),
            drain_revision: AtomicU64::new(0),
            reader_state: Arc::new(AtomicU64::new(RESUME)),
            views: Arc::downgrade(&self.views),
            retirement: watch::channel(None).0,
            reading: AtomicBool::new(false),
            handle: tokio::runtime::Handle::current(),
            #[cfg(test)]
            cleanup_confirmations: AtomicU64::new(0),
        });
        self.start_attachment(generation, view.clone(), allocation, child, reader, sender)?;
        view.trace_pressure("attached");
        #[cfg(test)]
        {
            let gate = self.startup_gate.lock().unwrap().take();
            if let Some(gate) = gate {
                gate.park(view_id).await;
            }
        }
        if request_control {
            let active = Arc::downgrade(&view);
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
        Ok(Attachment {
            view_id: view_id.to_string(),
            cols: size.cols,
            rows: size.rows,
            status,
        })
    }

    fn reserve(&self, generation: u64) -> Result<Allocation> {
        let mut views = self.views.lock().unwrap();
        ensure!(
            generation == self.generation.load(Ordering::SeqCst),
            "View context changed"
        );
        ensure!(
            views.allocated < VIEW_LIMIT,
            "At most 8 local views may be attached"
        );
        views.allocated += 1;
        Ok(Allocation(self.views.clone()))
    }

    fn start_attachment(
        &self,
        generation: u64,
        view: Arc<View>,
        allocation: Allocation,
        child: Box<dyn Child + Send + Sync>,
        reader: Box<dyn Read + Send>,
        sender: mpsc::Sender<PtyEvent>,
    ) -> Result<()> {
        let mut views = self.views.lock().unwrap();
        let current = generation == self.generation.load(Ordering::SeqCst);
        views.live.insert(view.terminal.view_id, view.clone());
        let producer = view.clone();
        let (start, begin) = std::sync::mpsc::channel();
        let reader = if !current || view.client_pid == 0 {
            drop(reader);
            drop(sender);
            Err(if !current {
                anyhow::anyhow!("View context changed")
            } else {
                anyhow::anyhow!("Attachment client identity unavailable")
            })
        } else {
            std::thread::Builder::new()
                .spawn(move || {
                    if begin.recv().is_ok() {
                        read_pty(
                            reader,
                            &producer.reader_state,
                            |event| producer.publish(&sender, event),
                            |reason| producer.retire(reason),
                        );
                    }
                })
                .context("Start attachment reader")
        };
        let (reader, error) = match reader {
            Ok(reader) => (Some(reader), None),
            Err(error) => (None, Some(error)),
        };
        let owner = view.clone();
        tokio::task::spawn_blocking(move || supervise(owner, child, reader, allocation));
        if let Some(error) = error {
            let _ = view.retire_locked(&mut views, Retirement::Failed);
            return Err(error);
        }
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

    pub fn attachment_pid(&self, view_id: &str) -> Result<u32> {
        Ok(self.view(view_id)?.client_pid)
    }

    pub async fn status(&self, view_id: &str) -> Result<TerminalStatus> {
        let view = self.view(view_id)?;
        ensure!(!view.closed(), "View detached");
        terminal_status(&view).await
    }

    pub async fn read(&self, view_id: &str) -> Result<Chunk> {
        self.read_with_clock(view_id, Instant::now).await
    }

    async fn read_with_clock(&self, view_id: &str, now: impl Fn() -> Instant) -> Result<Chunk> {
        let id = Uuid::parse_str(view_id).context("View detached")?;
        let view = {
            let mut views = self.views.lock().unwrap();
            views.prune(Instant::now());
            let Some(view) = views.live.get(&id) else {
                let index = views
                    .notices
                    .iter()
                    .position(|notice| notice.view == id)
                    .context("View detached")?;
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
                _ = view.retired() => Err(anyhow::anyhow!("View detached")),
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
            ensure!(lease.epoch == epoch, "Resize authority changed");
            (lease.token, lease.resize_sequence)
        };
        let reply = request(
            &view.directory,
            &Request::ResizeTerminal {
                terminal: view.terminal.clone(),
                token,
                sequence,
                size,
            },
        )
        .await;
        let mut control = view.control.lock().await;
        ensure!(!view.closed(), "View detached");
        let lease = control
            .lease
            .as_mut()
            .filter(|lease| lease.token == token)
            .context("Resize authority changed")?;
        match reply {
            Ok(Response::Resize {
                resize: ResizeResult::Committed { admission, status },
            }) if status.terminal == view.terminal
                && status.epoch == Some(epoch)
                && status.committed.intent == Some(admission)
                && admission.epoch == epoch
                && status.committed.size == size =>
            {
                lease.resize_sequence = sequence
                    .checked_add(1)
                    .context("Resize sequence exhausted")?;
                Ok(ResizeResult::Committed { admission, status })
            }
            Ok(Response::Resize {
                resize: ResizeResult::Uncertain { terminal, intent },
            }) if terminal == view.terminal && intent.epoch == epoch => {
                control.lease = None;
                Ok(ResizeResult::Uncertain { terminal, intent })
            }
            Ok(Response::Resize {
                resize: ResizeResult::Rejected { terminal, intent },
            }) if terminal == view.terminal && intent.epoch == epoch => {
                control.lease = None;
                Ok(ResizeResult::Rejected { terminal, intent })
            }
            Ok(Response::Error { message }) => bail!("{message}"),
            _ => {
                control.lease = None;
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
        let view = self.view(view_id)?;
        let mut output = view
            .output
            .try_lock()
            .context("Terminal boundary already in progress")?;
        let event = output
            .pending
            .as_ref()
            .context("No pending geometry boundary")?;
        ensure!(
            !view.closed()
                && !acknowledgement.renderer_id.is_nil()
                && event.status.epoch == acknowledgement.epoch
                && event.status.committed.revision == acknowledgement.revision
                && acknowledgement.size == event.status.committed.size,
            "Terminal renderer acknowledgement is stale or mismatched"
        );
        let redraw = async {
            let remaining =
                drain_budget(output.drain_started, Instant::now())?.context(DRAIN_TIMEOUT_ERROR)?;
            tokio::time::timeout(remaining, redraw_view(&view, event))
                .await
                .context(DRAIN_TIMEOUT_ERROR)?
        };
        let result = tokio::select! {
            biased;
            _ = view.retired() => Err(anyhow::anyhow!("View detached")),
            result = redraw => result,
        };
        match result {
            Ok(event) => {
                let _views = self.views.lock().unwrap();
                view.reader_state
                    .compare_exchange(PAUSE, RESUME, Ordering::AcqRel, Ordering::Acquire)
                    .map_err(|_| anyhow::anyhow!("View detached"))?;
                output.pending = None;
                output.applied_revision = acknowledgement.revision;
                output.drain_started = None;
                view.drain_revision.store(0, Ordering::Relaxed);
                Ok(event)
            }
            Err(error) => {
                let reason = if error.to_string() == DRAIN_TIMEOUT_ERROR {
                    Retirement::Typed(TerminalClosure::GeometryTimeout)
                } else {
                    Retirement::Failed
                };
                view.retire(reason);
                Err(error)
            }
        }
    }

    pub async fn input(&self, view_id: &str, data: Vec<u8>, paste: bool) -> Result<()> {
        ensure!(
            !data.is_empty() && data.len() <= 4096,
            "Input must contain 1..4096 bytes"
        );
        let view = self.view(view_id)?;
        let mut control = view.control.lock().await;
        ensure!(!view.closed(), "View detached");
        let lease = control.lease.as_mut().context("Керування ще не отримано")?;
        let token = lease.token;
        let sequence = lease.input_sequence;
        lease.input_sequence += 1;
        let runtime_id = view.terminal.target.runtime_id;
        let result = request(
            &view.directory,
            &Request::Input {
                runtime_id,
                token,
                sequence,
                data,
                paste,
            },
        )
        .await;
        if matches!(result, Ok(Response::Accepted {})) {
            return Ok(());
        }
        control.lease = None;
        drop(control);
        view.release(token).await;
        match result {
            Ok(Response::Error { message }) => bail!("{message}"),
            Err(error) => Err(error.context("Input outcome unknown; input was not retried")),
            _ => bail!("Unexpected input response; input was not retried"),
        }
    }
    pub fn detach(&self, view_id: &str) {
        if let Ok(view) = self.view(view_id) {
            view.retire(Retirement::Typed(TerminalClosure::Detached));
        }
    }

    pub fn detach_all(&self) {
        let mut views = self.views.lock().unwrap();
        self.generation.fetch_add(1, Ordering::SeqCst);
        let live = views.live.values().cloned().collect::<Vec<_>>();
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
    while [ ! -f "$root/redraw" ]; do /bin/sleep 0.01; done
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
            loop {
                let chunk = bridge.read(key).await.unwrap();
                assert!(!chunk.closed);
                output.extend(chunk.data);
                if chunk.writable && output == b"initial stream\x1b[" {
                    break;
                }
            }
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
