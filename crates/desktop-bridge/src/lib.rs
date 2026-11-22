//! Local desktop views with host-enforced exclusive input leases.
mod pending_create;

use anyhow::{Context, Result, bail, ensure};
pub use pending_create::{
    CreateDisposition, CreateSubmission, LocalCreate, LocalCreateStatus, ProviderReadiness,
};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
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
    collections::HashMap,
    io::{ErrorKind, Read},
    os::fd::BorrowedFd,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::mpsc;
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
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    output: tokio::sync::Mutex<Output>,
    reader_state: Arc<AtomicU64>,
}
impl View {
    fn closed(&self) -> bool {
        self.reader_state.load(Ordering::Acquire) == CLOSE
    }

    fn close(&self) {
        if self.reader_state.swap(CLOSE, Ordering::AcqRel) != CLOSE {
            let control = self.control.clone();
            let directory = self.directory.clone();
            let runtime_id = self.terminal.target.runtime_id;
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    let mut control = control.lock().await;
                    if let Some(lease) = control.lease.take() {
                        let _ = request(
                            &directory,
                            &Request::Release {
                                runtime_id,
                                token: lease.token,
                            },
                        )
                        .await;
                    }
                });
            }

            if let Ok(mut killer) = self.killer.lock() {
                let _ = killer.kill();
            }
            if let Ok(mut master) = self.master.lock() {
                master.take();
            }
        }
    }
}
impl Drop for View {
    fn drop(&mut self) {
        self.close();
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

fn read_pty(mut reader: Box<dyn Read + Send>, sender: mpsc::Sender<PtyEvent>, state: &AtomicU64) {
    let mut bytes = [0_u8; 8192];
    loop {
        match state.load(Ordering::Acquire) {
            CLOSE => return,
            PAUSE => std::thread::sleep(Duration::from_millis(5)),
            _ => match reader.read(&mut bytes) {
                Ok(0) => return,
                Ok(count) => {
                    if sender
                        .blocking_send(PtyEvent::Bytes(bytes[..count].to_vec()))
                        .is_err()
                    {
                        return;
                    }
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    if state
                        .compare_exchange(DRAIN, PAUSE, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                        && sender.blocking_send(PtyEvent::Boundary).is_err()
                    {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return,
            },
        }
    }
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
        Ok(Response::TerminalControl { token, epoch })
            if epoch.agent == view.terminal.agent_epoch =>
        {
            control.lease = Some(NativeLease {
                token,
                epoch,
                input_sequence: 0,
                resize_sequence: 0,
            });
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
            view.close();
            bail!("Terminal attachment identity changed")
        }
        _ => bail!("Розмір термінала недоступний"),
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

async fn read_chunk(view: &View, output: &mut Output) -> Result<Chunk> {
    ensure!(
        output.pending.is_none(),
        "Terminal grid acknowledgement required"
    );
    let mut chunk = Chunk {
        writable: false,
        data: Vec::new(),
        closed: view.closed(),
        status: None,
        geometry: None,
    };
    if chunk.closed {
        return Ok(chunk);
    }
    if Instant::now() >= output.refresh_at {
        match terminal_status(view).await {
            Ok(status) if status.committed.revision >= output.status.committed.revision => {
                output.status = status
            }
            Ok(_) => {}
            Err(_) if view.closed() => {
                chunk.closed = true;
                return Ok(chunk);
            }
            Err(_) => {
                output.status.provider = None;
                output.status.observation = ObservationState::Unavailable;
            }
        }
        output.refresh_at = Instant::now() + Duration::from_millis(150);
        chunk.status = Some(output.status.clone());
    }
    if output.status.committed.revision > output.applied_revision
        && view
            .reader_state
            .compare_exchange(RESUME, DRAIN, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    {
        output.drain_started = Some(Instant::now());
    }
    ensure!(
        output
            .drain_started
            .is_none_or(|start| start.elapsed() < Duration::from_secs(2)),
        "Terminal output did not reach a geometry boundary"
    );
    match tokio::time::timeout(Duration::from_millis(150), output.receiver.recv()).await {
        Ok(Some(PtyEvent::Bytes(data))) => chunk.data = data,
        Ok(Some(PtyEvent::Boundary)) => {
            let status = terminal_status(view).await?;
            ensure!(
                status.committed.revision >= output.status.committed.revision,
                "Terminal geometry revision went backwards"
            );
            let event = prepare_grid(view, status).await?;
            output.status = event.status.clone();
            output.pending = Some(event.clone());
            chunk.status = None;
            chunk.geometry = Some(event);
        }
        Ok(None) => {
            chunk.closed = true;
            chunk.writable = false;
        }
        Err(_) => {}
    }
    if let Ok(control) = view.control.try_lock() {
        output.writable = control
            .lease
            .as_ref()
            .is_some_and(|lease| Some(lease.epoch) == output.status.epoch);
    }
    chunk.writable = !chunk.closed && output.writable;
    Ok(chunk)
}

pub struct DesktopBridge {
    generation: AtomicU64,
    directory: Option<PathBuf>,
    tmux: PathBuf,
    views: Mutex<HashMap<String, Arc<View>>>,
    creates: tokio::sync::Mutex<()>,
}
impl DesktopBridge {
    pub fn new(directory: Option<PathBuf>, tmux: PathBuf) -> Self {
        Self {
            generation: AtomicU64::new(0),
            directory,
            tmux,
            views: Mutex::new(HashMap::new()),
            creates: tokio::sync::Mutex::new(()),
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
        let mut views = self
            .views
            .lock()
            .map_err(|_| anyhow::anyhow!("View lock unavailable"))?;
        ensure!(
            generation == self.generation.load(Ordering::SeqCst),
            "View context changed"
        );
        ensure!(views.len() < 8, "At most 8 local views may be attached");
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
        let mut child = pair.slave.spawn_command(command)?;
        let killer = child.clone_killer();
        let Some(client_pid) = child.process_id() else {
            let _ = child.kill();
            bail!("Attachment client identity unavailable");
        };
        drop(pair.slave);
        let (sender, receiver) = mpsc::channel(16);
        let reader_state = Arc::new(AtomicU64::new(RESUME));
        let state = reader_state.clone();
        std::thread::spawn(move || read_pty(reader, sender, &state));
        std::thread::spawn(move || {
            let _ = child.wait();
        });

        let view = Arc::new(View {
            terminal: status.terminal.clone(),
            directory: directory.clone(),
            tmux: self.tmux.clone(),
            client_pid,
            tty,
            control: Arc::new(tokio::sync::Mutex::new(Control { lease: None })),
            resize: tokio::sync::Mutex::new(()),
            killer: Mutex::new(killer),
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
            reader_state,
        });
        if request_control {
            let active = Arc::downgrade(&view);
            tokio::spawn(async move {
                while let Some(view) = active.upgrade() {
                    if view.closed() {
                        break;
                    }
                    refresh_control(&view).await;
                    drop(view);
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            });
        }
        let view_id = view_id.to_string();
        views.insert(view_id.clone(), view);
        Ok(Attachment {
            view_id,
            cols: size.cols,
            rows: size.rows,
            status,
        })
    }

    fn view(&self, view_id: &str) -> Result<Arc<View>> {
        self.views
            .lock()
            .map_err(|_| anyhow::anyhow!("View lock unavailable"))?
            .get(view_id)
            .cloned()
            .context("View detached")
    }

    pub async fn status(&self, view_id: &str) -> Result<TerminalStatus> {
        let view = self.view(view_id)?;
        ensure!(!view.closed(), "View detached");
        terminal_status(&view).await
    }

    pub async fn read(&self, view_id: &str) -> Result<Chunk> {
        let view = self.view(view_id)?;
        let mut output = view
            .output
            .try_lock()
            .context("Terminal read already in progress")?;
        let result = read_chunk(&view, &mut output).await;
        if result.as_ref().is_ok_and(|chunk| chunk.closed) || result.is_err() {
            view.close();
        }
        result
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
                let _ = request(
                    &view.directory,
                    &Request::Release {
                        runtime_id: view.terminal.target.runtime_id,
                        token,
                    },
                )
                .await;
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
        let redraw = redraw_view(&view, event).await;
        match redraw {
            Ok(event) => {
                output.pending = None;
                output.applied_revision = acknowledgement.revision;
                output.drain_started = None;
                view.reader_state
                    .compare_exchange(PAUSE, RESUME, Ordering::AcqRel, Ordering::Acquire)
                    .map_err(|_| anyhow::anyhow!("View detached"))?;
                Ok(event)
            }
            Err(error) => {
                view.close();
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
        let _ = request(&view.directory, &Request::Release { runtime_id, token }).await;
        match result {
            Ok(Response::Error { message }) => bail!("{message}"),
            Err(error) => Err(error.context("Input outcome unknown; input was not retried")),
            _ => bail!("Unexpected input response; input was not retried"),
        }
    }
    pub fn detach(&self, view_id: &str) {
        if let Ok(mut views) = self.views.lock()
            && let Some(view) = views.remove(view_id)
        {
            view.close();
        }
    }
    pub fn detach_all(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut views) = self.views.lock() {
            for (_, view) in views.drain() {
                view.close();
            }
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
    async fn w1_resize_private_ipc_admission_and_ordered_independent_grids() {
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
    printf 'initial stream'
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
                if chunk.writable && output == b"initial stream" {
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

            let event = loop {
                let chunk = bridge.read(key).await.unwrap();
                assert!(!chunk.closed);
                if let Some(event) = chunk.geometry {
                    break event;
                }
            };
            assert_eq!(event.status.terminal, view.status.terminal);
            assert_eq!(event.client.size, desired);
            assert_eq!(event.bridge_pty.size, desired);
            assert!(!fixture.0.join("redraw").exists());
            let acknowledgement = GridAcknowledgement {
                renderer_id: Uuid::new_v4(),
                epoch: Some(epoch),
                revision: 1,
                size: desired,
                focused: true,
            };
            assert!(
                bridge
                    .acknowledge_grid(
                        key,
                        GridAcknowledgement {
                            revision: 0,
                            ..acknowledgement
                        }
                    )
                    .await
                    .is_err()
            );
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
            bridge.detach(key);
            assert!(bridge.resize(key, desired, epoch).await.is_err());
        })
        .await
        .unwrap();
        server.abort();
    }
    #[tokio::test]
    async fn w1_resize_reader_barrier_follows_buffered_bytes_and_pauses_until_acknowledged() {
        struct Fragments(usize);
        impl Read for Fragments {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                self.0 += 1;
                let next: &[u8] = match self.0 {
                    1 => &[0xe6, 0xbc],
                    2 => &[0xa2, 0x1b, b'['],
                    3 => return Err(ErrorKind::WouldBlock.into()),
                    4 => b"H",
                    _ => return Ok(0),
                };
                bytes[..next.len()].copy_from_slice(next);
                Ok(next.len())
            }
        }

        let state = Arc::new(AtomicU64::new(DRAIN));
        let (sender, mut events) = mpsc::channel(1);
        let reader = state.clone();
        let producer =
            std::thread::spawn(move || read_pty(Box::new(Fragments(0)), sender, &reader));
        tokio::time::timeout(Duration::from_secs(2), async {
            assert!(matches!(events.recv().await, Some(PtyEvent::Bytes(bytes)) if bytes == [0xe6, 0xbc]));
            assert!(matches!(events.recv().await, Some(PtyEvent::Bytes(bytes)) if bytes == [0xa2, 0x1b, b'[']));
            assert!(matches!(events.recv().await, Some(PtyEvent::Boundary)));
            assert_eq!(state.load(Ordering::Acquire), PAUSE);
            assert!(events.try_recv().is_err());
            state.store(RESUME, Ordering::Release);
            assert!(matches!(events.recv().await, Some(PtyEvent::Bytes(bytes)) if bytes == b"H"));
            assert!(events.recv().await.is_none());
        }).await.unwrap();
        producer.join().unwrap();
    }
}
