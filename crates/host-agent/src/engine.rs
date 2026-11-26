use crate::{
    catalog::{
        CatalogLane, Operation, OperationStage, PaneIdentity, ServerGeneration, StoredOutcome,
    },
    model::{
        CommittedGeometry, CreateState, INPUT_CHUNK_LIMIT, INPUT_LEASE_TTL, LaunchOutcome,
        ObservationState, PASTE_LIMIT, PaneLiveness, PasteAdmission, PasteBinding, PasteResult,
        ProductIssue, ProviderProfile, ProviderSample, Request, ResizeEpoch, ResizeIntent,
        ResizeResult, Response, Runtime, RuntimeOrigin, TerminalSize, TerminalStatus,
        TerminalTarget, TerminalView, parse_name, tmux_name,
    },
    product_launch::{LAUNCH_COMMAND, LaunchGuard, NONCE_ENV, read_setup_fault, workspace_locator},
};
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{io::AsyncWriteExt, process::Command};
use uuid::Uuid;

pub struct Engine {
    tmux: PathBuf,
    socket: PathBuf,
    agent_epoch: Uuid,
    ownership: Arc<Mutex<crate::ownership::Ownership>>,
    lanes: Mutex<HashMap<Uuid, Arc<tokio::sync::Mutex<ResizeLane>>>>,
    #[cfg(test)]
    paste_test: PasteTest,
}

#[derive(Default)]
struct ResizeLane {
    confirmed: Option<(TerminalTarget, ResizeEpoch)>,
    reconcile: bool,
    pending_paste: Option<PendingPaste>,
    paste_quarantined: bool,
}

struct PendingPaste {
    binding: PasteBinding,
    expected: GeometryReadback,
    receipt: Uuid,
    released: bool,
}

struct GuardedPaste {
    pane: String,
    guard: String,
    branch: String,
    otherwise: String,
    enqueued: String,
    rejected: String,
}

fn valid_paste(data: &[u8], expected: usize) -> bool {
    data.len() == expected
        && (1..=PASTE_LIMIT).contains(&expected)
        && std::str::from_utf8(data).is_ok_and(|text| {
            text.chars()
                .all(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        })
}

#[cfg(test)]
#[derive(Default)]
struct PasteTest {
    now: Mutex<Option<Instant>>,
    admission: Mutex<Option<Arc<PasteBarrier>>>,
    sender_fixture: bool,
    provider_sample: Option<ProviderSample>,
    send_watchdog: Option<Duration>,
}

#[cfg(test)]
#[derive(Default)]
struct PasteBarrier {
    reached: tokio::sync::Notify,
    proceed: tokio::sync::Notify,
}

#[cfg(test)]
impl PasteTest {
    async fn pause_before_admission(&self) {
        let barrier = self.admission.lock().unwrap().take();
        if let Some(barrier) = barrier {
            barrier.reached.notify_one();
            barrier.proceed.notified().await;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GeometryReadback {
    target: TerminalTarget,
    server: ServerGeneration,
    name: String,
    nonce: String,
    tty: PathBuf,
    epoch: Option<ResizeEpoch>,
    committed: CommittedGeometry,
    paste_launch: String,
    paste_marker: String,
    sender_supported: bool,
    input_enabled: bool,
}

const EPOCH_OPTION: &str = "@rs_resize_epoch";
const INTENT_OPTION: &str = "@rs_resize_intent";
const PASTE_LAUNCH_OPTION: &str = "@rs_paste_launch";
const PASTE_OPTION: &str = "@rs_paste_admission";
const PASTE_WATCHDOG: Duration = Duration::from_millis(650);
const GEOMETRY_FORMAT: &str = concat!(
    "#{pid}|#{start_time}|#{@rs_create_generation}|#{session_id}|#{window_id}|#{pane_id}|#{pane_pid}|",
    "#{session_name}|#{window_index}|#{pane_index}|#{@rs_create_nonce}|#{pane_dead}|",
    "#{session_windows}|#{window_panes}|#{window-size}|#{pane_tty}|",
    "#{pane_width}|#{pane_height}|#{window_width}|#{window_height}|",
    "#{@rs_resize_epoch}|#{@rs_resize_intent}|",
    "#{?#{m/r:^[0-9a-f-]*$,#{@rs_paste_launch}},#{@rs_paste_launch},invalid}|",
    "#{?#{m/r:^[a-z0-9:~-]*$,#{@rs_paste_admission}},#{@rs_paste_admission},invalid}|",
    "#{pane_input_off}"
);
const MAX_REVISION: u64 = (1 << 53) - 1;
const GENERATION_OPTION: &str = "@rs_create_generation";
const GENERATION_FORMAT: &str = "#{pid}|#{start_time}|#{@rs_create_generation}";
const PANE_OPTION: &str = "@rs_create_nonce";
const RECEIPT_FORMAT: &str = "#{session_id}|#{window_id}|#{pane_id}|#{pane_pid}";
const EVIDENCE_FORMAT: &str = concat!(
    "#{pid}|#{start_time}|#{@rs_create_generation}|",
    "#{session_id}|#{window_id}|#{pane_id}|#{pane_pid}|",
    "#{session_name}|#{window_index}|#{pane_index}|#{@rs_create_nonce}|",
    "#{pane_dead}|#{pane_width}|#{pane_height}"
);
const RUNTIME_LIMIT: usize = 16;

struct Observation {
    identity: PaneIdentity,
    runtime: Runtime,
}

struct PaneEvidence {
    identity: PaneIdentity,
    pane_pid: Option<u32>,
    exited: bool,
    width: u16,
    height: u16,
}

fn unknown(issue: impl Into<Option<ProductIssue>>) -> CreateState {
    CreateState::Unknown {
        issue: issue.into(),
    }
}

fn unavailable() -> ProductIssue {
    ProductIssue::TmuxUnavailable
}

fn control_hold() -> Response {
    control_recovering(PasteAdmission::PriorSendUnknown)
}

fn control_recovering(paste_admission: PasteAdmission) -> Response {
    Response::ControlRecovering { paste_admission }
}

fn server_fields(line: &str) -> Option<(u32, u64, &str)> {
    let mut fields = line.split('|');
    let pid = fields.next()?.parse().ok().filter(|pid| *pid > 0)?;
    let start = fields.next()?.parse().ok().filter(|start| *start > 0)?;
    let token = fields.next()?;
    fields.next().is_none().then_some((pid, start, token))
}

fn generation(line: &str) -> Option<ServerGeneration> {
    let (pid, start, token) = server_fields(line)?;
    let token = Uuid::try_parse(token)
        .ok()
        .filter(|token| !token.is_nil())?;
    Some(ServerGeneration { pid, start, token })
}

fn pane_identity(fields: &[&str]) -> Option<PaneIdentity> {
    let [session, window, pane, pid] = fields else {
        return None;
    };
    for (value, prefix) in [(session, '$'), (window, '@'), (pane, '%')] {
        value.strip_prefix(prefix)?.parse::<u32>().ok()?;
    }
    Some(PaneIdentity {
        session: (*session).into(),
        window: (*window).into(),
        pane: (*pane).into(),
        pid: pid.parse().ok().filter(|pid| *pid > 0)?,
    })
}

// @security: Escape argument bytes so tmux parses path syntax and metacharacters as data.
fn tmux_word(value: &str) -> String {
    let escaped: String = value
        .bytes()
        .map(|byte| match byte {
            b'/' | b'.' | b'_' | b'-' | b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' => {
                char::from(byte).to_string()
            }
            _ => format!("\\{byte:03o}"),
        })
        .collect();
    format!("\"{escaped}\"")
}

fn parse_uuid(value: &str) -> Result<Uuid> {
    let id = Uuid::parse_str(value)?;
    ensure!(
        !id.is_nil() && id.to_string() == value,
        "Invalid epoch or intent"
    );
    Ok(id)
}

fn epoch_text(epoch: Option<ResizeEpoch>) -> String {
    epoch.map_or_else(String::new, |epoch| {
        format!("{}:{}", epoch.agent, epoch.owner)
    })
}

fn parse_epoch(value: &str) -> Result<Option<ResizeEpoch>> {
    if value.is_empty() {
        return Ok(None);
    }
    let (agent, owner) = value.split_once(':').context("Malformed engine epoch")?;
    Ok(Some(ResizeEpoch {
        agent: parse_uuid(agent)?,
        owner: parse_uuid(owner)?,
    }))
}

fn intent_text(committed: CommittedGeometry) -> String {
    committed.intent.map_or_else(String::new, |intent| {
        format!(
            "{}:{}:{}:{}:{}",
            epoch_text(Some(intent.epoch)),
            intent.id,
            committed.revision,
            committed.size.cols,
            committed.size.rows
        )
    })
}

fn parse_committed(marker: &str, size: TerminalSize) -> Result<CommittedGeometry> {
    if marker.is_empty() {
        return Ok(CommittedGeometry {
            size,
            revision: 0,
            intent: None,
        });
    }
    let mut parts = marker.rsplitn(5, ':');
    let rows: u16 = parts.next().context("Missing intent rows")?.parse()?;
    let cols: u16 = parts.next().context("Missing intent columns")?.parse()?;
    let revision = parts.next().context("Missing intent revision")?.parse()?;
    let id = parse_uuid(parts.next().context("Missing intent ID")?)?;
    let epoch = parse_epoch(parts.next().context("Missing intent epoch")?)?
        .context("Missing intent epoch")?;
    ensure!(
        size == (TerminalSize { cols, rows }) && (1..=MAX_REVISION).contains(&revision),
        "Intent marker does not match the virtual grid"
    );
    let committed = CommittedGeometry {
        size,
        revision,
        intent: Some(ResizeIntent { epoch, id }),
    };
    ensure!(intent_text(committed) == marker, "Malformed intent marker");
    Ok(committed)
}

fn format_equal(field: &str, value: impl std::fmt::Display) -> String {
    format!("#{{==:#{{{field}}},{value}}}")
}

fn format_all(predicates: impl IntoIterator<Item = String>) -> String {
    fn combine(values: &[String]) -> String {
        if let [value] = values {
            return value.clone();
        }
        let (left, right) = values.split_at(values.len() / 2);
        format!("#{{&&:{},{}}}", combine(left), combine(right))
    }
    let values: Vec<_> = predicates.into_iter().collect();
    assert!(!values.is_empty());
    combine(&values)
}

fn pane_witness(pane: &str, witness: &str) -> String {
    format!("display-message -p -l -t {pane} {witness}")
}

fn rejection_witness(pane: &str, witness: &str) -> String {
    let guard = format_all([
        format_equal("pane_id", pane),
        format_equal("after-display-message", ""),
        format_equal("command-error", ""),
    ]);
    format!(
        "if-shell -F -t {pane} {} {}",
        tmux_word(&guard),
        tmux_word(&pane_witness(pane, witness))
    )
}

impl GeometryReadback {
    fn same_pane(&self, other: &Self) -> bool {
        self.target == other.target
            && self.server == other.server
            && self.name == other.name
            && self.nonce == other.nonce
            && self.tty == other.tty
            && self.paste_launch == other.paste_launch
    }

    fn paste_prefix(&self, epoch: Option<ResizeEpoch>) -> String {
        let t = &self.target;
        let launch = self.nonce.split_once(':').map_or("-", |(nonce, _)| nonce);
        format!(
            "v1~{}~{}~{}~{}~{}~{}~{}~{}~{}~{}~{}~{}~",
            t.runtime_id,
            t.generation,
            self.server.pid,
            self.server.start,
            &t.session[1..],
            &t.window[1..],
            &t.pane[1..],
            t.pid,
            t.process_start,
            launch,
            self.paste_launch,
            epoch_text(epoch),
        )
    }

    fn ready_marker(&self, epoch: Option<ResizeEpoch>) -> String {
        format!("{}r~{}", self.paste_prefix(epoch), Uuid::new_v4())
    }

    fn paste_ready(&self) -> bool {
        parse_uuid(&self.paste_launch).is_ok()
            && self
                .paste_marker
                .strip_prefix(&format!("{}r~", self.paste_prefix(self.epoch)))
                .is_some_and(|nonce| parse_uuid(nonce).is_ok())
    }

    fn paste_admission_hold(&self) -> Option<PasteAdmission> {
        if self.paste_ready() {
            return (!self.sender_supported).then_some(PasteAdmission::Unavailable);
        }
        if !self.sender_supported && self.paste_launch.is_empty() && self.paste_marker.is_empty() {
            return Some(PasteAdmission::Unavailable);
        }
        Some(PasteAdmission::PriorSendUnknown)
    }

    fn guard(&self, predecessor: Option<ResizeEpoch>) -> String {
        format_all([
            format_equal("pid", self.server.pid),
            format_equal("start_time", self.server.start),
            format_equal(GENERATION_OPTION, self.target.generation),
            format_equal("session_name", &self.name),
            format_equal("session_id", &self.target.session),
            format_equal("window_id", &self.target.window),
            format_equal("pane_id", &self.target.pane),
            format_equal("pane_pid", self.target.pid),
            format_equal("pane_tty", self.tty.display()),
            format_equal(PANE_OPTION, &self.nonce),
            format_equal("session_windows", 1),
            format_equal("window_panes", 1),
            format_equal("pane_dead", 0),
            format_equal("window-size", "manual"),
            format_equal("after-set-option", ""),
            format_equal("after-resize-window", ""),
            format_equal("after-display-message", ""),
            format_equal("command-error", ""),
            format_equal(EPOCH_OPTION, epoch_text(predecessor)),
        ])
    }

    fn status(&self, terminal: TerminalView, provider: Option<ProviderSample>) -> TerminalStatus {
        let observation = match provider {
            Some(sample) if sample.size == self.committed.size => ObservationState::Observed,
            Some(_) => ObservationState::Pending,
            None => ObservationState::Unavailable,
        };
        TerminalStatus {
            terminal,
            epoch: self.epoch,
            committed: self.committed,
            provider,
            observation,
        }
    }
}

#[cfg(target_os = "macos")]
fn process_start(pid: u32) -> Result<u64> {
    use nix::libc;
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let length = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    let read = unsafe {
        libc::proc_pidinfo(
            i32::try_from(pid)?,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            length,
        )
    };
    ensure!(read == length, "Process identity unavailable");
    let info = unsafe { info.assume_init() };
    ensure!(
        info.pbi_pid == pid && info.pbi_start_tvsec > 0,
        "Process identity changed"
    );
    Ok(info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
}

#[cfg(target_os = "linux")]
fn process_start(pid: u32) -> Result<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let (_, fields) = stat
        .rsplit_once(") ")
        .context("Process identity unavailable")?;
    Ok(fields
        .split_whitespace()
        .nth(19)
        .context("Missing process start")?
        .parse()?)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_start(_: u32) -> Result<u64> {
    bail!("Process identity unavailable on this platform")
}

fn tty_identity(metadata: &fs::Metadata) -> Result<(u64, u64, u64)> {
    ensure!(
        metadata.file_type().is_char_device() && metadata.uid() == nix::unistd::geteuid().as_raw(),
        "Unexpected provider terminal device"
    );
    Ok((metadata.dev(), metadata.ino(), metadata.rdev()))
}

fn sample_tty(path: &Path) -> Result<(ProviderSample, (u64, u64, u64))> {
    use nix::libc;
    let tty = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)?;
    let identity = tty_identity(&tty.metadata()?)?;
    let mut size = std::mem::MaybeUninit::<libc::winsize>::uninit();
    let result = unsafe { libc::ioctl(tty.as_raw_fd(), libc::TIOCGWINSZ, size.as_mut_ptr()) };
    let sampled_at_us = u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_micros())?;
    if result == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    let size = unsafe { size.assume_init() };
    let size = TerminalSize {
        cols: size.ws_col,
        rows: size.ws_row,
    };
    ensure!(size.supported(), "Provider grid outside supported range");
    Ok((
        ProviderSample {
            size,
            sampled_at_us,
        },
        identity,
    ))
}

#[doc(hidden)]
pub fn set_local_pty_nonblocking(fd: std::os::fd::BorrowedFd<'_>) -> std::io::Result<()> {
    use nix::libc;
    let fd = fd.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

impl Engine {
    pub(crate) fn new(tmux: PathBuf, socket: PathBuf) -> Self {
        Self {
            tmux,
            socket,
            agent_epoch: Uuid::new_v4(),
            ownership: Arc::default(),
            lanes: Mutex::default(),
            #[cfg(test)]
            paste_test: PasteTest::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_fixture_sender(mut self) -> Self {
        self.paste_test.sender_fixture = true;
        self
    }

    fn now(&self) -> Instant {
        #[cfg(test)]
        if let Some(now) = *self.paste_test.now.lock().unwrap() {
            return now;
        }
        Instant::now()
    }

    fn sender_guard(&self) -> String {
        let Some(path) = self.socket.to_str().filter(|path| {
            path.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"/-._ ".contains(&c))
        }) else {
            return "0".into();
        };
        format_all([
            format_equal("version", "3.8"),
            format_equal("socket_path", path),
            format_equal("after-load-buffer", ""),
            format_equal("after-save-buffer", ""),
            format_equal("after-paste-buffer", ""),
            format_equal("after-delete-buffer", ""),
            format_equal("after-send-keys", ""),
            format_equal("after-set-option", ""),
            format_equal("after-display-message", ""),
            format_equal("command-error", ""),
        ])
    }

    fn private_sender_socket(&self) -> bool {
        #[cfg(test)]
        if self.paste_test.sender_fixture {
            return true;
        }
        self.socket.is_absolute()
            && self
                .socket
                .file_name()
                .is_some_and(|name| name == "tmux.sock")
            && self.socket.parent().is_some_and(|parent| {
                let uid = nix::unistd::geteuid().as_raw();
                crate::validate_directory(parent).is_ok()
                    && fs::symlink_metadata(&self.socket)
                        .is_ok_and(|meta| meta.file_type().is_socket() && meta.uid() == uid)
            })
    }

    fn paste_guard(&self, read: &GeometryReadback) -> String {
        format_all([
            read.guard(read.epoch),
            self.sender_guard(),
            format_equal(PASTE_LAUNCH_OPTION, &read.paste_launch),
            format_equal(PASTE_OPTION, &read.paste_marker),
        ])
    }

    fn expire_writer(
        &self,
        runtime: Uuid,
        token: Uuid,
        lane: &Arc<tokio::sync::Mutex<ResizeLane>>,
    ) {
        let owners = Arc::downgrade(&self.ownership);
        let lane = Arc::downgrade(lane);
        tokio::spawn(async move {
            let mut deadline = Instant::now() + INPUT_LEASE_TTL;
            loop {
                tokio::time::sleep_until(deadline.into()).await;
                let (Some(owners), Some(lane)) = (owners.upgrade(), lane.upgrade()) else {
                    break;
                };
                let lane = lane.lock().await;
                let mut owners = owners.lock().unwrap();
                let Some(current) = owners.deadline(runtime, token) else {
                    break;
                };
                if current <= Instant::now() {
                    owners.discard_stage(runtime, token);
                    if lane.pending_paste.is_none() {
                        owners.release(runtime, token);
                    }
                    break;
                }
                deadline = current;
            }
        });
    }

    fn owners(&self) -> std::sync::MutexGuard<'_, crate::ownership::Ownership> {
        self.ownership.lock().expect("ownership state poisoned")
    }

    fn lane(&self, runtime: Uuid) -> Arc<tokio::sync::Mutex<ResizeLane>> {
        self.lanes
            .lock()
            .expect("runtime lanes poisoned")
            .entry(runtime)
            .or_default()
            .clone()
    }

    fn existing_lane(&self, runtime: Uuid) -> Option<Arc<tokio::sync::Mutex<ResizeLane>>> {
        self.lanes
            .lock()
            .expect("runtime lanes poisoned")
            .get(&runtime)
            .cloned()
    }

    async fn command(&self, args: &[&str]) -> Result<String> {
        self.bounded_command(args, Duration::from_secs(3)).await
    }

    async fn geometry_command(&self, args: &[&str]) -> Result<String> {
        self.bounded_command(args, self.terminal_command_watchdog())
            .await
    }

    fn terminal_command_watchdog(&self) -> Duration {
        #[cfg(test)]
        if self.paste_test.sender_fixture {
            return Duration::from_secs(3);
        }
        PASTE_WATCHDOG
    }

    async fn bounded_command(&self, args: &[&str], wait: Duration) -> Result<String> {
        // Never start tmux here: its lifetime belongs to a separate launchd job.
        let output = tokio::time::timeout(
            wait,
            Command::new(&self.tmux)
                .args(["-N", "-S"])
                .arg(&self.socket)
                .args(args)
                .env_remove("TMUX")
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .context("tmux command timed out; outcome may be unknown, reconcile before retry")??;
        ensure!(
            output.status.success(),
            "tmux unavailable or operation failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        ensure!(
            output.stdout.len() <= 1_000_000,
            "tmux output exceeds spike limit"
        );
        String::from_utf8(output.stdout).context("tmux returned non-UTF8 metadata")
    }

    pub async fn list(&self) -> Result<Vec<Runtime>> {
        // list-panes needs a target even with -a when the live server is empty.
        // list-sessions distinguishes that valid state from an unavailable server.
        if self
            .command(&["list-sessions", "-F", "#{session_name}"])
            .await?
            .is_empty()
        {
            return Ok(Vec::new());
        }
        let output = self.command(&["list-panes", "-a", "-F", "#{session_name}\t#{pane_id}\t#{pane_pid}\t#{pane_dead}\t#{pane_width}\t#{pane_height}\t#{?@rs_command,#{@rs_command},#{pane_current_command}}"]).await?;
        let mut runtimes = Vec::new();
        for line in output.lines() {
            let fields: Vec<_> = line.split('\t').collect();
            ensure!(fields.len() == 7, "unexpected tmux metadata");
            let Some((session_id, runtime_id)) = parse_name(fields[0]) else {
                continue;
            };
            ensure!(
                !runtimes
                    .iter()
                    .any(|r: &Runtime| r.runtime_id == runtime_id),
                "multiple panes for one runtime are unsupported"
            );
            runtimes.push(Runtime {
                session_id,
                runtime_id,
                tmux_name: fields[0].into(),
                pane_id: fields[1].into(),
                command: fields[6].into(),
                pane_pid: if fields[2].is_empty() {
                    None
                } else {
                    Some(fields[2].parse()?)
                },
                exited: fields[3] == "1",
                width: fields[4].parse()?,
                height: fields[5].parse()?,
                origin: RuntimeOrigin::Legacy,
                workspace_id: None,
                operation_id: None,
            });
        }
        Ok(runtimes)
    }

    async fn generation(&self) -> Result<ServerGeneration> {
        let output = self
            .command(&["display-message", "-p", GENERATION_FORMAT])
            .await?;
        let (pid, start, token) =
            server_fields(output.trim_end()).context("invalid tmux generation")?;
        if !token.is_empty() {
            return generation(output.trim_end()).context("invalid tmux generation token");
        }
        let token = Uuid::new_v4();
        // @invariant: The tmux server checks its identity and the empty token before setting the token.
        let guard = format!("#{{&&:#{{==:#{{pid}},{pid}}},#{{==:#{{start_time}},{start}}}}}");
        let guard = format!("#{{&&:{guard},#{{==:#{{{GENERATION_OPTION}}},}}}}");
        let set = format!("set-option -s {GENERATION_OPTION} {token}");
        self.command(&["if-shell", "-F", &guard, &set]).await?;
        let output = self
            .command(&["display-message", "-p", GENERATION_FORMAT])
            .await?;
        let captured = generation(output.trim_end()).context("tmux generation unavailable")?;
        ensure!(
            (captured.pid, captured.start, captured.token) == (pid, start, token),
            "tmux generation changed"
        );
        Ok(captured)
    }

    async fn names(&self, expected: ServerGeneration) -> Result<Vec<String>> {
        let output = self
            .command(&[
                "display-message",
                "-p",
                GENERATION_FORMAT,
                ";",
                "list-sessions",
                "-F",
                "#{session_name}",
            ])
            .await?;
        let mut lines = output.lines();
        ensure!(
            lines.next().and_then(generation) == Some(expected),
            "tmux generation changed"
        );
        Ok(lines
            .filter(|name| parse_name(name).is_some())
            .map(str::to_owned)
            .collect())
    }

    async fn observe(&self, operation: &Operation) -> Result<Observation> {
        let pane = self
            .observe_pane(
                &operation.target,
                operation.generation,
                &operation.nonce.to_string(),
            )
            .await?;
        Ok(Observation {
            runtime: Runtime {
                session_id: operation.session_id,
                runtime_id: operation.runtime_id,
                tmux_name: operation.target.clone(),
                pane_id: pane.identity.pane.clone(),
                command: operation.profile.executable_name().into(),
                pane_pid: pane.pane_pid,
                exited: pane.exited,
                width: pane.width,
                height: pane.height,
                origin: RuntimeOrigin::Managed,
                workspace_id: Some(operation.workspace_id),
                operation_id: Some(operation.id),
            },
            identity: pane.identity,
        })
    }

    async fn observe_pane(
        &self,
        name: &str,
        server: ServerGeneration,
        nonce: &str,
    ) -> Result<PaneEvidence> {
        let session_target = format!("={name}");
        let window_target = format!("{session_target}:0");
        let output = self
            .command(&[
                "show-environment",
                "-t",
                &session_target,
                NONCE_ENV,
                ";",
                "list-panes",
                "-s",
                "-t",
                &window_target,
                "-F",
                EVIDENCE_FORMAT,
            ])
            .await?;
        let mut lines = output.lines();
        ensure!(
            lines.next() == Some(&format!("{NONCE_ENV}={nonce}")),
            "missing session launch nonce"
        );
        let row = lines.next().context("missing launch pane")?;
        ensure!(lines.next().is_none(), "multiple candidate panes");
        let fields: Vec<_> = row.split('|').collect();
        ensure!(fields.len() == 14, "invalid launch evidence");
        ensure!(
            generation(&fields[..3].join("|")) == Some(server),
            "tmux generation changed"
        );
        let (stamped_nonce, stamp) = fields[10].split_once(':').context("missing pane nonce")?;
        let stamp: Vec<_> = stamp.split(':').collect();
        let identity = pane_identity(&stamp).context("invalid launch pane stamp")?;
        ensure!(
            fields[7] == name
                && fields[8..10] == ["0", "0"]
                && stamped_nonce == nonce
                && fields[3..6] == stamp[..3],
            "mismatched launch target or pane nonce"
        );
        let exited = match fields[11] {
            "0" => false,
            "1" => true,
            _ => bail!("invalid pane liveness"),
        };
        // @invariant: A retained dead pane may omit its current PID. Its launch stamp still binds the original PID.
        let pane_pid = match fields[6] {
            "" if exited => None,
            pid if pid.parse::<u32>() == Ok(identity.pid) => Some(identity.pid),
            _ => bail!("mismatched or missing pane PID"),
        };
        Ok(PaneEvidence {
            identity,
            pane_pid,
            exited,
            width: fields[12].parse()?,
            height: fields[13].parse()?,
        })
    }

    fn spawn_command(&self, operation: &Operation) -> Result<(String, String)> {
        let service_state = self
            .socket
            .parent()
            .context("missing service state")?
            .to_owned();
        let launcher = std::env::current_exe()?;
        let launcher = workspace_locator(&launcher)
            .map_err(|_| anyhow::anyhow!("unsupported host binary path"))?;
        let cwd = workspace_locator(Path::new(&operation.policy.workspace.canonical))
            .map_err(|_| anyhow::anyhow!("unsupported launch directory"))?;
        let capsule = LaunchGuard {
            locator: operation.locator.clone(),
            profile: operation.profile,
            policy: operation.policy.clone(),
            service_state,
            nonce: operation.nonce,
        }
        .argument()?;
        let ServerGeneration { pid, start, token } = operation.generation;
        let guard = format!("#{{&&:#{{==:#{{pid}},{pid}}},#{{==:#{{start_time}},{start}}}}}");
        let guard = format!("#{{&&:{guard},#{{==:#{{{GENERATION_OPTION}}},{token}}}}}");
        let name = &operation.target;
        let pane_target = format!("={name}:0.0");
        let nonce = operation.nonce;
        // @security: Double literal hashes to prevent tmux format expansion in the working directory.
        let cwd = tmux_word(&cwd.replace('#', "##"));
        let launcher = tmux_word(launcher);
        let profile = operation.profile.executable_name();
        let branch = format!(
            "new-session -d -E -s {name} -n {profile} -c {cwd} -x 100 -y 30 \
             -e {NONCE_ENV}={nonce} -P -F '{RECEIPT_FORMAT}' -- {launcher} {LAUNCH_COMMAND} {capsule} ; \
             set-option -Fp -t {pane_target} {PANE_OPTION} '{nonce}:#{{session_id}}:#{{window_id}}:#{{pane_id}}:#{{pane_pid}}' ; \
             set-option -p -t {pane_target} @rs_command {profile}"
        );
        Ok((guard, branch))
    }

    async fn load_operation(
        catalog: &CatalogLane,
        key: Uuid,
    ) -> Result<Option<Operation>, ProductIssue> {
        catalog.run(move |db, _| db.operation(key)).await
    }

    pub(crate) async fn create(
        &self,
        catalog: &CatalogLane,
        key: Uuid,
        workspace_id: Uuid,
        profile: ProviderProfile,
        retry: Option<Uuid>,
    ) -> CreateState {
        if key.is_nil() || workspace_id.is_nil() {
            return CreateState::Failed {
                issue: ProductIssue::InvalidRequest,
            };
        }
        let existing = match Self::load_operation(catalog, key).await {
            Ok(record) => record,
            Err(issue) => return unknown(issue),
        };
        if let Some(operation) = existing {
            if (operation.workspace_id, operation.profile) != (workspace_id, profile)
                || retry.is_some_and(|id| id != operation.runtime_id)
            {
                return CreateState::Failed {
                    issue: ProductIssue::CreateConflict,
                };
            }
            if operation.stage == OperationStage::Reserved {
                return self.dispatch_reserved(catalog, operation).await;
            }
            return self.reconcile(catalog, operation).await;
        }
        if retry.is_some() {
            return unknown(None);
        }
        let (locator, policy) = match catalog
            .run(move |db, profiles| db.preflight(workspace_id, profile, profiles))
            .await
        {
            Ok(prepared) => prepared,
            Err(issue)
                if issue.storage_fault() || issue == ProductIssue::WorkspaceCheckTimedOut =>
            {
                return unknown(issue);
            }
            Err(issue) => return CreateState::Failed { issue },
        };
        let Ok(generation) = self.generation().await else {
            return unknown(unavailable());
        };
        let session_id = Uuid::new_v4();
        let runtime_id = Uuid::new_v4();
        let operation = Operation {
            key,
            id: Uuid::new_v4(),
            workspace_id,
            profile,
            session_id,
            runtime_id,
            target: tmux_name(session_id, runtime_id),
            nonce: Uuid::new_v4(),
            generation,
            locator,
            policy,
            stage: OperationStage::Reserved,
        };
        match catalog.run(move |db, _| db.reserve(&operation)).await {
            Ok(true) => {}
            Ok(false) => return unknown(None),
            Err(issue) => return unknown(issue),
        }
        match Self::load_operation(catalog, key).await {
            Ok(Some(operation)) => self.dispatch_reserved(catalog, operation).await,
            Ok(None) => unknown(None),
            Err(issue) => unknown(issue),
        }
    }

    async fn dispatch_reserved(&self, catalog: &CatalogLane, operation: Operation) -> CreateState {
        let key = operation.key;
        let runtime_id = operation.runtime_id;
        let generation = operation.generation;
        let Ok(names) = self.names(generation).await else {
            return unknown(unavailable());
        };
        if names.contains(&operation.target) {
            return unknown(None);
        }
        let targets = match catalog
            .run(move |db, _| db.marked_targets(generation))
            .await
        {
            Ok(targets) => targets,
            Err(issue) => return unknown(issue),
        };
        if names.len()
            + targets
                .iter()
                .filter(|target| !names.contains(target))
                .count()
            >= RUNTIME_LIMIT
        {
            return Self::fail_reserved(catalog, key, runtime_id, ProductIssue::RuntimeLimit).await;
        }
        let Ok((guard, branch)) = self.spawn_command(&operation) else {
            return Self::fail_reserved(catalog, key, runtime_id, ProductIssue::LaunchSetupFailed)
                .await;
        };
        let (workspace_id, profile) = (operation.workspace_id, operation.profile);
        let policy = operation.policy.clone();
        let prepared = catalog
            .run(move |db, profiles| {
                let (_, current) = db.preflight(workspace_id, profile, profiles)?;
                if current != policy {
                    return Err(ProductIssue::ProviderNotExecutable);
                }
                Ok(())
            })
            .await;
        if let Err(issue) = prepared {
            if issue.storage_fault() || issue == ProductIssue::WorkspaceCheckTimedOut {
                return unknown(issue);
            }
            return Self::fail_reserved(catalog, key, runtime_id, issue).await;
        }
        // @invariant: Only the caller with an acknowledged durable marker write may dispatch the spawn.
        match catalog.run(move |db, _| db.mark(key, runtime_id)).await {
            Ok(true) => {}
            Ok(false) => return unknown(None),
            Err(issue) => return unknown(issue),
        }
        let Ok(receipt) = self.command(&["if-shell", "-F", &guard, &branch]).await else {
            return unknown(unavailable());
        };
        let receipt: Vec<_> = receipt.trim_end().split('|').collect();
        let Some(receipt) = pane_identity(&receipt) else {
            return unknown(None);
        };
        match self.observe(&operation).await {
            Ok(observed) if observed.identity == receipt => {
                let _ = self
                    .stamp_paste(
                        &observed.runtime,
                        generation,
                        &receipt,
                        Some(operation.nonce),
                    )
                    .await;
                self.finalize_launch(catalog, &operation, observed).await
            }
            _ => unknown(None),
        }
    }

    async fn fail_reserved(
        catalog: &CatalogLane,
        key: Uuid,
        id: Uuid,
        issue: ProductIssue,
    ) -> CreateState {
        match catalog
            .run(move |db, _| db.finalize(key, id, &StoredOutcome::Failed(issue)))
            .await
        {
            Ok(true) => CreateState::Failed { issue },
            Ok(false) => unknown(None),
            Err(issue) => unknown(issue),
        }
    }

    pub(crate) async fn create_status(&self, catalog: &CatalogLane, key: Uuid) -> CreateState {
        match Self::load_operation(catalog, key).await {
            Ok(Some(operation)) => self.reconcile(catalog, operation).await,
            Ok(None) => CreateState::Absent {},
            Err(issue) => unknown(issue),
        }
    }

    async fn reconcile(&self, catalog: &CatalogLane, operation: Operation) -> CreateState {
        match &operation.stage {
            OperationStage::Reserved => match self.names(operation.generation).await {
                Ok(names) if !names.contains(&operation.target) => CreateState::Pending {
                    reservation: operation.runtime_id,
                },
                Ok(_) => unknown(None),
                Err(_) => unknown(unavailable()),
            },
            OperationStage::Marked => match self.observe(&operation).await {
                Ok(observed) => self.finalize_launch(catalog, &operation, observed).await,
                Err(_) => unknown(None),
            },
            OperationStage::Finished(StoredOutcome::Failed(issue)) => {
                CreateState::Failed { issue: *issue }
            }
            OperationStage::Finished(StoredOutcome::Launched { identity, outcome }) => {
                let current = match self.observe(&operation).await {
                    Ok(observed) if observed.identity == *identity => liveness(&observed.runtime),
                    _ => PaneLiveness::Unavailable,
                };
                let setup_issue = if current == PaneLiveness::Exited {
                    self.socket
                        .parent()
                        .and_then(|state| read_setup_fault(state, operation.nonce))
                } else {
                    None
                }
                .or(outcome.setup_issue);
                CreateState::Confirmed {
                    outcome: outcome.as_ref().clone(),
                    current,
                    setup_issue,
                }
            }
        }
    }

    async fn finalize_launch(
        &self,
        catalog: &CatalogLane,
        operation: &Operation,
        observed: Observation,
    ) -> CreateState {
        let Some(state) = self.socket.parent() else {
            return unknown(None);
        };
        let setup_issue = if observed.runtime.exited {
            read_setup_fault(state, operation.nonce)
        } else {
            None
        };
        let current = liveness(&observed.runtime);
        let outcome = LaunchOutcome {
            runtime: observed.runtime,
            setup_issue,
        };
        let stored = StoredOutcome::Launched {
            identity: observed.identity,
            outcome: Box::new(outcome.clone()),
        };
        let (key, id) = (operation.key, operation.runtime_id);
        match catalog
            .run(move |db, _| db.finalize(key, id, &stored))
            .await
        {
            Ok(true) => CreateState::Confirmed {
                outcome,
                current,
                setup_issue,
            },
            Ok(false) => unknown(None),
            Err(issue) => unknown(issue),
        }
    }

    pub(crate) async fn inventory(&self, catalog: Option<&CatalogLane>) -> Result<Vec<Runtime>> {
        let mut runtimes = self.list().await?;
        if let Some(catalog) = catalog {
            let ids: Vec<_> = runtimes.iter().map(|runtime| runtime.runtime_id).collect();
            if !ids.is_empty()
                && let Ok(operations) = catalog.run(move |db, _| db.by_runtime(&ids)).await
            {
                // @contract: Catalog or identity faults leave the underlying tmux inventory visible.
                for operation in operations {
                    if let OperationStage::Finished(StoredOutcome::Launched { identity, .. }) =
                        &operation.stage
                        && let Some(runtime) = runtimes.iter_mut().find(|runtime| {
                            runtime.runtime_id == operation.runtime_id
                                && runtime.session_id == operation.session_id
                                && runtime.pane_id == identity.pane
                        })
                        && let Ok(observed) = self.observe(&operation).await
                        && observed.identity == *identity
                    {
                        *runtime = observed.runtime;
                    }
                }
            }
        }
        Ok(runtimes)
    }

    async fn geometry(&self, runtime_id: Uuid, pane: &str) -> Result<GeometryReadback> {
        let format = format!("{GEOMETRY_FORMAT}|{}", self.sender_guard());
        let output = self
            .geometry_command(&["display-message", "-p", "-t", pane, &format])
            .await?;
        let mut lines = output.lines();
        let fields: Vec<_> = lines
            .next()
            .context("Missing terminal target")?
            .split('|')
            .collect();
        ensure!(
            lines.next().is_none() && fields.len() == 26,
            "Invalid terminal metadata"
        );
        let server = generation(&fields[..3].join("|")).context("Invalid terminal generation")?;
        let identity = pane_identity(&fields[3..7]).context("Invalid terminal pane")?;
        ensure!(
            parse_name(fields[7]).is_some_and(|(_, id)| id == runtime_id)
                && fields[8..10] == ["0", "0"]
                && fields[11..15] == ["0", "1", "1", "manual"],
            "Terminal target changed or is unsupported"
        );
        let stamp = fields[10];
        if !stamp.is_empty() {
            let (nonce, ids) = stamp
                .split_once(':')
                .context("Invalid terminal launch stamp")?;
            parse_uuid(nonce)?;
            let parsed: Vec<_> = ids.split(':').collect();
            ensure!(
                pane_identity(&parsed) == Some(identity.clone()),
                "Terminal launch stamp changed"
            );
        }
        let tty = fields[15];
        ensure!(
            (tty.strip_prefix("/dev/ttys")
                .or_else(|| tty.strip_prefix("/dev/pts/")))
            .is_some_and(
                |suffix| !suffix.is_empty() && suffix.bytes().all(|c| c.is_ascii_alphanumeric())
            ),
            "Unsupported terminal device"
        );
        let size = TerminalSize {
            cols: fields[16].parse()?,
            rows: fields[17].parse()?,
        };
        ensure!(
            size.supported()
                && size.cols == fields[18].parse::<u16>()?
                && size.rows == fields[19].parse::<u16>()?,
            "Window and pane grids disagree or are unsupported"
        );
        let target = TerminalTarget {
            runtime_id,
            generation: server.token,
            session: identity.session,
            window: identity.window,
            pane: identity.pane,
            pid: identity.pid,
            process_start: process_start(identity.pid)?,
        };
        let epoch = parse_epoch(fields[20])?;
        let committed = parse_committed(fields[21], size)?;
        ensure!(
            committed.intent.is_none() || epoch.is_some(),
            "Resize intent has no engine epoch"
        );
        Ok(GeometryReadback {
            target,
            server,
            name: fields[7].to_owned(),
            nonce: stamp.to_owned(),
            tty: tty.into(),
            epoch,
            committed,
            paste_launch: fields[22].into(),
            paste_marker: fields[23].into(),
            input_enabled: fields[24] == "0",
            sender_supported: matches!(fields[24], "0" | "1")
                && fields[25] == "1"
                && self.private_sender_socket(),
        })
    }

    async fn geometry_for_runtime(&self, runtime: &Runtime) -> Result<GeometryReadback> {
        let server = self.generation().await?;
        let read = self.geometry(runtime.runtime_id, &runtime.pane_id).await?;
        ensure!(
            !runtime.exited
                && read.target.pane == runtime.pane_id
                && Some(read.target.pid) == runtime.pane_pid
                && read.server == server
                && read.name == runtime.tmux_name,
            "Terminal identity changed"
        );
        Ok(read)
    }

    async fn terminal_operation(
        read: &GeometryReadback,
        catalog: Option<&CatalogLane>,
    ) -> Result<Option<Operation>, ProductIssue> {
        let id = read.target.runtime_id;
        match catalog {
            Some(lane) => lane
                .run(move |db, _| db.by_runtime(&[id]))
                .await
                .map(|mut operations| operations.pop()),
            None => Err(ProductIssue::CatalogUnconfigured),
        }
    }

    async fn corroborate_terminal(
        &self,
        read: &GeometryReadback,
        nonce: &str,
    ) -> Result<PaneIdentity> {
        ensure!(
            read.nonce
                .split_once(':')
                .is_some_and(|(stamp, _)| stamp == nonce),
            "Managed terminal launch stamp changed"
        );
        let evidence = self.observe_pane(&read.name, read.server, nonce).await?;
        let identity = evidence.identity;
        let target = &read.target;
        ensure!(
            !evidence.exited
                && identity.session == target.session
                && identity.window == target.window
                && identity.pane == target.pane
                && evidence.pane_pid == Some(target.pid)
                && process_start(target.pid)? == target.process_start,
            "Managed terminal identity changed"
        );
        Ok(identity)
    }

    async fn verify_terminal_operation(
        &self,
        read: &GeometryReadback,
        operation: Option<Operation>,
    ) -> Result<()> {
        let Some(operation) = operation else {
            ensure!(read.nonce.is_empty(), "Managed terminal is uncorroborated");
            return Ok(());
        };
        let OperationStage::Finished(StoredOutcome::Launched { identity, .. }) = &operation.stage
        else {
            bail!("Managed launch is unconfirmed");
        };
        ensure!(
            read.server == operation.generation
                && read.name == operation.target
                && self
                    .corroborate_terminal(read, &operation.nonce.to_string())
                    .await?
                    == *identity,
            "Managed terminal identity changed"
        );
        Ok(())
    }

    async fn observe_terminal(
        &self,
        read: &GeometryReadback,
        catalog: Option<&CatalogLane>,
    ) -> Result<()> {
        match Self::terminal_operation(read, catalog).await {
            Ok(operation) => self.verify_terminal_operation(read, operation).await,
            Err(_) => {
                if let Some((nonce, _)) = read.nonce.split_once(':') {
                    self.corroborate_terminal(read, nonce).await?;
                }
                Ok(())
            }
        }
    }

    async fn authorize_terminal_writer(
        &self,
        read: &GeometryReadback,
        catalog: Option<&CatalogLane>,
    ) -> Result<()> {
        match Self::terminal_operation(read, catalog).await {
            Ok(operation) => self.verify_terminal_operation(read, operation).await,
            Err(_) if read.nonce.is_empty() => Ok(()),
            Err(issue) => bail!("Managed terminal catalog unavailable: {issue:?}"),
        }
    }

    async fn scoped_geometry(&self, terminal: &TerminalView) -> Result<GeometryReadback> {
        ensure!(
            terminal.agent_epoch == self.agent_epoch && !terminal.view_id.is_nil(),
            "Terminal view changed"
        );
        let read = self
            .geometry(terminal.target.runtime_id, &terminal.target.pane)
            .await?;
        ensure!(read.target == terminal.target, "Terminal target changed");
        Ok(read)
    }

    async fn sample_provider(&self, before: &GeometryReadback) -> Result<ProviderSample> {
        #[cfg(test)]
        let simulated_sample = self.paste_test.provider_sample;
        #[cfg(not(test))]
        let simulated_sample = None;
        let (sample, device) = match simulated_sample {
            Some(sample) => (sample, None),
            None => {
                let (sample, device) = sample_tty(&before.tty)?;
                (sample, Some(device))
            }
        };
        let after = self
            .geometry(before.target.runtime_id, &before.target.pane)
            .await?;
        ensure!(*before == after, "Provider sample lost its target identity");
        if let Some(device) = device {
            ensure!(
                tty_identity(&fs::symlink_metadata(&after.tty)?)? == device,
                "Provider sample lost its target identity"
            );
        }
        Ok(sample)
    }

    async fn stamp_paste(
        &self,
        runtime: &Runtime,
        server: ServerGeneration,
        identity: &PaneIdentity,
        managed_nonce: Option<Uuid>,
    ) -> Result<()> {
        let read = self.geometry(runtime.runtime_id, &identity.pane).await?;
        let nonce = managed_nonce.map_or_else(String::new, |nonce| {
            format!(
                "{nonce}:{}:{}:{}:{}",
                identity.session, identity.window, identity.pane, identity.pid
            )
        });
        ensure!(
            read.sender_supported
                && read.input_enabled
                && !runtime.exited
                && read.server == server
                && read.name == runtime.tmux_name
                && read.target.session == identity.session
                && read.target.window == identity.window
                && read.target.pane == identity.pane
                && read.target.pid == identity.pid
                && read.nonce == nonce
                && read.epoch.is_none()
                && read.paste_launch.is_empty()
                && read.paste_marker.is_empty(),
            "Paste launch provenance is unavailable"
        );
        self.prove_paste_buffer_io().await?;

        let guard = format_all([self.paste_guard(&read), format_equal("pane_input_off", 0)]);
        let launch = Uuid::new_v4().to_string();
        let mut stamped = read.clone();
        stamped.paste_launch.clone_from(&launch);
        let ready = stamped.ready_marker(None);
        let pane = &read.target.pane;
        let branch = format!(
            "set-option -p -o -t {pane} {PASTE_LAUNCH_OPTION} {launch} ; set-option -p -o -t {pane} {PASTE_OPTION} {ready}",
        );
        self.geometry_command(&["if-shell", "-F", "-t", pane, &guard, &branch])
            .await?;
        let after = self.geometry(runtime.runtime_id, pane).await?;
        ensure!(
            after.same_pane(&stamped)
                && after.epoch.is_none()
                && after.sender_supported
                && after.paste_marker == ready,
            "Paste launch provenance is unconfirmed"
        );
        Ok(())
    }

    async fn prove_paste_buffer_io(&self) -> Result<()> {
        let commands = self.geometry_command(&["list-commands"]).await?;
        ensure!(
            commands
                .lines()
                .any(|line| line.starts_with("paste-buffer ") && line.contains("[-dprS]"))
                && commands
                    .lines()
                    .any(|line| line.starts_with("load-buffer ")),
            "Exact paste sender is unavailable"
        );
        let name = format!("rs-paste-probe-{}", Uuid::new_v4().simple());
        let sample = format!("{}\n\t\u{2603}", Uuid::new_v4());
        let loaded = self.load_paste(&name, sample.as_bytes()).await;
        let returned = match loaded {
            Ok(()) => {
                self.geometry_command(&["save-buffer", "-b", &name, "-"])
                    .await
            }
            Err(error) => Err(error),
        };
        let deleted = self.geometry_command(&["delete-buffer", "-b", &name]).await;
        ensure!(returned? == sample, "Paste sender stdin readback differs");
        deleted?;
        Ok(())
    }

    async fn transition_marker(
        &self,
        from: &GeometryReadback,
        next: &str,
    ) -> Result<Option<GeometryReadback>> {
        let pane = &from.target.pane;
        let witness = Uuid::new_v4();
        let printed = pane_witness(pane, &witness.to_string());
        let branch = format!("set-option -p -t {pane} {PASTE_OPTION} {next} ; {printed}");
        let rejected = format!("rs-rejected-{witness}");
        let else_branch = rejection_witness(pane, &rejected);
        let reply = self
            .geometry_command(&[
                "if-shell",
                "-F",
                "-t",
                pane,
                &self.paste_guard(from),
                &branch,
                &else_branch,
            ])
            .await;
        if reply
            .as_ref()
            .is_ok_and(|reply| reply.trim_end() == rejected)
        {
            return Ok(None);
        }
        // @contract: An exact readback can resolve a lost no-effect transition reply.
        let after = self.geometry(from.target.runtime_id, pane).await?;
        ensure!(
            after.same_pane(from)
                && after.epoch == from.epoch
                && after.sender_supported
                && after.paste_marker == next,
            "Paste marker transition unconfirmed"
        );
        Ok(Some(after))
    }

    async fn load_paste(&self, name: &str, data: &[u8]) -> Result<()> {
        tokio::time::timeout(self.terminal_command_watchdog(), async {
            let mut child = Command::new(&self.tmux)
                .args(["-N", "-S"])
                .arg(&self.socket)
                .args(["load-buffer", "-b", name, "-"])
                .env_remove("TMUX")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()?;
            child
                .stdin
                .take()
                .context("Paste buffer stdin unavailable")?
                .write_all(data)
                .await?;
            ensure!(child.wait().await?.success(), "Paste buffer load failed");
            Ok(())
        })
        .await
        .context("Paste buffer load timed out")?
    }

    async fn delete_paste(&self, buffer: &str) {
        let _ = self
            .geometry_command(&["delete-buffer", "-b", buffer])
            .await;
    }

    async fn paste_stage(
        &self,
        binding: PasteBinding,
        offset: usize,
        data: Vec<u8>,
        catalog: Option<&CatalogLane>,
    ) -> PasteResult {
        let runtime = binding.terminal.target.runtime_id;
        let Some(lock) = self.existing_lane(runtime) else {
            return PasteResult::Stale {};
        };
        let lane = lock.lock().await;
        if lane.pending_paste.is_some() || lane.paste_quarantined {
            return PasteResult::Unknown {};
        }
        let rejected = match self.scoped_geometry(&binding.terminal).await {
            Err(_) => Some(PasteResult::Stale {}),
            Ok(read)
                if self
                    .authorize_terminal_writer(&read, catalog)
                    .await
                    .is_err() =>
            {
                Some(PasteResult::Unavailable {})
            }
            Ok(read) => match read.paste_admission_hold() {
                Some(PasteAdmission::PriorSendUnknown) => Some(PasteResult::Unknown {}),
                Some(PasteAdmission::Unavailable) => Some(PasteResult::Unavailable {}),
                None if read.epoch != Some(binding.epoch) => Some(PasteResult::Stale {}),
                None if !read.input_enabled => Some(PasteResult::Unavailable {}),
                None => None,
            },
        };
        if let Some(result) = rejected {
            self.owners().discard_rejected_stage(&binding, self.now());
            return result;
        }
        self.owners().stage(binding, offset, &data, self.now())
    }

    fn quarantine(&self, lane: &mut ResizeLane, binding: &PasteBinding) -> PasteResult {
        lane.paste_quarantined = true;
        self.owners()
            .release(binding.terminal.target.runtime_id, binding.token);
        PasteResult::Unknown {}
    }

    async fn reject_reserved(
        &self,
        lane: &mut ResizeLane,
        binding: &PasteBinding,
        held: &GeometryReadback,
        buffer: &str,
        result: PasteResult,
    ) -> PasteResult {
        let ready = held.ready_marker(held.epoch);
        let disarmed = self.transition_marker(held, &ready).await;
        self.delete_paste(buffer).await;
        if matches!(disarmed, Ok(Some(_))) {
            result
        } else {
            self.quarantine(lane, binding)
        }
    }

    async fn paste_commit(
        &self,
        binding: PasteBinding,
        expected: usize,
        catalog: Option<&CatalogLane>,
    ) -> PasteResult {
        let runtime = binding.terminal.target.runtime_id;
        let Some(lock) = self.existing_lane(runtime) else {
            return PasteResult::Stale {};
        };
        let mut lane = lock.lock().await;
        if lane.pending_paste.is_some() || lane.paste_quarantined {
            return PasteResult::Unknown {};
        }
        let Some(data) = self.owners().take_stage(&binding) else {
            return PasteResult::Stale {};
        };
        if !self.owners().paste_live(&binding, self.now()) {
            return PasteResult::Stale {};
        }
        if !valid_paste(&data, expected) {
            return PasteResult::Invalid {};
        }
        let Ok(read) = self.scoped_geometry(&binding.terminal).await else {
            return PasteResult::Stale {};
        };
        if read.epoch != Some(binding.epoch) {
            return PasteResult::Stale {};
        }
        if self
            .authorize_terminal_writer(&read, catalog)
            .await
            .is_err()
        {
            return PasteResult::Unavailable {};
        }
        match read.paste_admission_hold() {
            Some(PasteAdmission::PriorSendUnknown) => return self.quarantine(&mut lane, &binding),
            Some(PasteAdmission::Unavailable) => return PasteResult::Unavailable {},
            None => {}
        }
        if !read.input_enabled {
            return PasteResult::Unavailable {};
        }
        let buffer = format!("rs-paste-{}", Uuid::new_v4().simple());
        let loaded = self.load_paste(&buffer, &data).await;
        drop(data);
        if loaded.is_err() {
            self.delete_paste(&buffer).await;
            return PasteResult::Unavailable {};
        }
        let nonce = Uuid::new_v4();
        let prefix = read.paste_prefix(read.epoch);
        let suffix = format!("{nonce}~{}~{buffer}", binding.operation);
        let armed = format!("{prefix}a~{suffix}");
        let spent = format!("{prefix}s~{suffix}");
        let mut held = match self.transition_marker(&read, &armed).await {
            Ok(Some(held)) => held,
            Ok(None) => {
                self.delete_paste(&buffer).await;
                return PasteResult::Stale {};
            }
            Err(_) => {
                self.delete_paste(&buffer).await;
                return self.quarantine(&mut lane, &binding);
            }
        };
        #[cfg(test)]
        self.paste_test.pause_before_admission().await;
        let current = self.scoped_geometry(&binding.terminal).await.ok();
        let authorized = match &current {
            Some(read) => self.authorize_terminal_writer(read, catalog).await.is_ok(),
            None => false,
        };
        let exact = current.as_ref().filter(|read| {
            authorized
                && read.same_pane(&held)
                && read.epoch == held.epoch
                && read.paste_marker == armed
                && read.sender_supported
        });
        let send = self.guarded_paste(&held, &spent, &buffer, nonce);
        // @invariant: The serialized monotonic check is the last admission step before submission.
        let rejection = match exact {
            Some(read) if self.owners().paste_live(&binding, self.now()) => {
                (!read.input_enabled).then_some(PasteResult::Unavailable {})
            }
            _ => Some(PasteResult::Stale {}),
        };
        if let Some(rejection) = rejection {
            return self
                .reject_reserved(&mut lane, &binding, &held, &buffer, rejection)
                .await;
        }
        match self.submit_paste(&send).await {
            Ok(reply) if reply.trim_end() == send.rejected => {
                self.reject_reserved(
                    &mut lane,
                    &binding,
                    &held,
                    &buffer,
                    PasteResult::Unavailable {},
                )
                .await
            }
            Ok(reply) if reply.trim_end() == send.enqueued => {
                if !self.owners().confirm_paste(&binding) {
                    return self.quarantine(&mut lane, &binding);
                }
                let receipt = Uuid::new_v4();
                held.paste_marker = spent;
                lane.pending_paste = Some(PendingPaste {
                    binding,
                    expected: held,
                    receipt,
                    released: false,
                });
                PasteResult::Committed { receipt }
            }
            _ => self.quarantine(&mut lane, &binding),
        }
    }

    fn guarded_paste(
        &self,
        held: &GeometryReadback,
        spent: &str,
        buffer: &str,
        nonce: Uuid,
    ) -> GuardedPaste {
        let pane = held.target.pane.clone();
        let guard = format_all([self.paste_guard(held), format_equal("pane_input_off", 0)]);
        let enqueued = format!("rs-paste-enqueued-{nonce}");
        let rejected = format!("rs-paste-rejected-{nonce}");
        let printed = pane_witness(&pane, &enqueued);
        let branch = format!(
            "set-option -p -t {pane} {PASTE_OPTION} {spent} ; paste-buffer -p -r -S -d -b {buffer} -t {pane} ; {printed}"
        );
        let otherwise = rejection_witness(&pane, &rejected);
        GuardedPaste {
            pane,
            guard,
            branch,
            otherwise,
            enqueued,
            rejected,
        }
    }

    async fn submit_paste(&self, send: &GuardedPaste) -> Result<String> {
        let watchdog = PASTE_WATCHDOG;
        #[cfg(test)]
        let watchdog = self.paste_test.send_watchdog.unwrap_or(watchdog);
        self.bounded_command(
            &[
                "if-shell",
                "-F",
                "-t",
                &send.pane,
                &send.guard,
                &send.branch,
                &send.otherwise,
            ],
            watchdog,
        )
        .await
    }

    async fn paste_ack(
        &self,
        binding: PasteBinding,
        receipt: Uuid,
        catalog: Option<&CatalogLane>,
    ) -> PasteResult {
        let runtime = binding.terminal.target.runtime_id;
        let Some(lock) = self.existing_lane(runtime) else {
            return PasteResult::Stale {};
        };
        let mut lane = lock.lock().await;
        let Some(pending) = lane.pending_paste.as_ref() else {
            return if lane.paste_quarantined {
                PasteResult::Unknown {}
            } else {
                PasteResult::Stale {}
            };
        };
        if pending.binding != binding || pending.receipt != receipt {
            return PasteResult::Stale {};
        }
        let pending = lane.pending_paste.take().expect("matched pending paste");
        let Ok(read) = self.scoped_geometry(&binding.terminal).await else {
            return self.quarantine(&mut lane, &binding);
        };
        if !read.sender_supported
            || !read.same_pane(&pending.expected)
            || read.epoch != Some(binding.epoch)
            || read.paste_marker != pending.expected.paste_marker
            || self
                .authorize_terminal_writer(&read, catalog)
                .await
                .is_err()
        {
            return self.quarantine(&mut lane, &binding);
        }
        let ready = read.ready_marker(read.epoch);
        if !matches!(self.transition_marker(&read, &ready).await, Ok(Some(_))) {
            return self.quarantine(&mut lane, &binding);
        }
        let mut owners = self.owners();
        let writable = !pending.released
            && owners
                .deadline(runtime, binding.token)
                .is_some_and(|deadline| deadline > self.now());
        if !writable {
            owners.release(runtime, binding.token);
        }
        PasteResult::Acknowledged { writable }
    }

    async fn rotate_epoch(
        &self,
        from: &GeometryReadback,
        successor: ResizeEpoch,
    ) -> Result<GeometryReadback> {
        let conditional_absence = if from.epoch.is_none() { "-o " } else { "" };
        let ready = from.ready_marker(Some(successor));
        let pane = &from.target.pane;
        let witness = pane_witness(pane, "rs-fenced");
        let branch = format!(
            "set-option -p {conditional_absence}-t {pane} {EPOCH_OPTION} {} ; set-option -p -t {pane} {PASTE_OPTION} {ready} ; {witness}",
            epoch_text(Some(successor)),
        );
        let reply = self
            .geometry_command(&[
                "if-shell",
                "-F",
                "-t",
                pane,
                &self.paste_guard(from),
                &branch,
            ])
            .await?;
        ensure!(
            reply.trim_end() == "rs-fenced",
            "Engine epoch predecessor changed"
        );
        let after = self
            .geometry(from.target.runtime_id, &from.target.pane)
            .await?;
        ensure!(
            after.same_pane(from)
                && after.epoch == Some(successor)
                && after.paste_marker == ready
                && after.paste_ready()
                && after.sender_supported,
            "Engine epoch transition is unconfirmed"
        );
        Ok(after)
    }

    async fn acquire_terminal(
        &self,
        runtime_id: Uuid,
        terminal: Option<TerminalView>,
        catalog: Option<&CatalogLane>,
    ) -> Result<Response> {
        let read_scope = async || match &terminal {
            Some(terminal) => self.scoped_geometry(terminal).await,
            None => {
                self.geometry_for_runtime(&self.find(runtime_id).await?)
                    .await
            }
        };
        let (lock, first_read) = match self.existing_lane(runtime_id) {
            Some(lock) => (lock, None),
            None => {
                let read = read_scope().await?;
                (self.lane(runtime_id), Some(read))
            }
        };
        let mut lane = lock.try_lock().context("Terminal mutation in progress")?;
        if lane.pending_paste.is_some() || lane.paste_quarantined {
            return Ok(control_hold());
        }
        let read = match first_read {
            Some(read) => read,
            None => read_scope().await?,
        };
        self.authorize_terminal_writer(&read, catalog).await?;
        if let Some(hold) = read.paste_admission_hold() {
            return Ok(control_recovering(hold));
        }
        self.owners().unowned(runtime_id, self.now())?;
        let recover = lane.reconcile
            || lane
                .confirmed
                .as_ref()
                .is_none_or(|(target, _)| *target != read.target);
        if !recover && lane.confirmed.as_ref().map(|(_, epoch)| *epoch) != read.epoch {
            lane.reconcile = true;
            bail!("Engine epoch changed and must be reconciled");
        }
        lane.reconcile = true;
        let epoch = ResizeEpoch {
            agent: self.agent_epoch,
            owner: Uuid::new_v4(),
        };
        let after = self.rotate_epoch(&read, epoch).await?;
        self.sample_provider(&after).await?;
        let native = terminal.is_some();
        let token = self
            .owners()
            .grant(runtime_id, terminal, Some(epoch), self.now())?;
        self.expire_writer(runtime_id, token, &lock);
        lane.confirmed = Some((after.target, epoch));
        lane.reconcile = false;
        if native {
            Ok(Response::TerminalControl { token, epoch })
        } else {
            Ok(Response::Control { token })
        }
    }

    async fn commit_geometry(
        &self,
        read: &GeometryReadback,
        committed: CommittedGeometry,
    ) -> Result<Option<GeometryReadback>> {
        let marker = intent_text(committed);
        let guard = format_all([
            read.guard(read.epoch),
            format_equal(INTENT_OPTION, intent_text(read.committed)),
            format_equal("window_width", read.committed.size.cols),
            format_equal("window_height", read.committed.size.rows),
        ]);
        // @invariant: The selected branch reaches resize and the correlated write in one non-yielding queue turn.
        let branch = format!(
            "resize-window -t {} -x {} -y {} ; set-option -p -t {} {INTENT_OPTION} {} ; display-message -p rs-committed",
            read.target.window, committed.size.cols, committed.size.rows, read.target.pane, marker
        );
        let reply = self
            .geometry_command(&["if-shell", "-F", "-t", &read.target.pane, &guard, &branch])
            .await?;
        if reply.is_empty() {
            return Ok(None);
        }
        ensure!(
            reply.trim_end() == "rs-committed",
            "Engine resize receipt is unconfirmed"
        );
        let after = self
            .geometry(read.target.runtime_id, &read.target.pane)
            .await?;
        ensure!(
            after.target == read.target
                && after.epoch == read.epoch
                && after.tty == read.tty
                && after.committed == committed,
            "Engine resize intent is unconfirmed"
        );
        Ok(Some(after))
    }

    async fn resize_terminal(
        &self,
        terminal: TerminalView,
        token: Uuid,
        sequence: u64,
        size: TerminalSize,
        catalog: Option<&CatalogLane>,
    ) -> Result<Response> {
        ensure!(
            size.supported(),
            "Terminal proposal outside supported range"
        );
        let runtime_id = terminal.target.runtime_id;
        let lane = self
            .lanes
            .lock()
            .expect("runtime lanes poisoned")
            .get(&runtime_id)
            .cloned()
            .context("Resize authority unavailable")?;
        let mut lane = lane.try_lock().context("Terminal mutation in progress")?;
        let read = self.scoped_geometry(&terminal).await?;
        let epoch = read.epoch.context("Engine epoch unavailable")?;
        ensure!(
            !lane.reconcile
                && lane.pending_paste.is_none()
                && !lane.paste_quarantined
                && read.paste_ready()
                && read.sender_supported
                && lane
                    .confirmed
                    .as_ref()
                    .is_some_and(
                        |(target, confirmed)| *target == read.target && *confirmed == epoch
                    ),
            "Resize authority unavailable"
        );
        self.authorize_terminal_writer(&read, catalog).await?;
        let revision = read.committed.revision + 1;
        ensure!(revision <= MAX_REVISION, "Terminal revision exhausted");
        self.owners()
            .resize(&terminal, token, epoch, sequence, self.now())?;
        lane.reconcile = true;
        let intent = ResizeIntent {
            epoch,
            id: Uuid::new_v4(),
        };
        let committed = CommittedGeometry {
            size,
            revision,
            intent: Some(intent),
        };
        let resize = match self.commit_geometry(&read, committed).await {
            Ok(Some(after)) => {
                lane.reconcile = false;
                let mut status = after.status(terminal, None);
                status.observation = ObservationState::Pending;
                ResizeResult::Committed {
                    admission: intent,
                    status,
                }
            }
            Ok(None) => {
                self.owners().release(runtime_id, token);
                ResizeResult::Rejected { terminal, intent }
            }
            Err(_) => {
                self.owners().release(runtime_id, token);
                ResizeResult::Uncertain { terminal, intent }
            }
        };
        Ok(Response::Resize { resize })
    }

    async fn find(&self, id: Uuid) -> Result<Runtime> {
        self.list()
            .await?
            .into_iter()
            .find(|r| r.runtime_id == id)
            .context("managed runtime not found; it may have ended with the tmux service")
    }

    async fn writer_geometry(
        &self,
        lane: &ResizeLane,
        runtime: Uuid,
        token: Uuid,
        catalog: Option<&CatalogLane>,
    ) -> Result<GeometryReadback> {
        let (target, epoch) = lane.confirmed.as_ref().context("Керування втрачено")?;
        ensure!(
            self.owners().epoch(runtime, token, self.now()) == Some(*epoch),
            "Керування втрачено"
        );
        let read = self.geometry(runtime, &target.pane).await?;
        ensure!(
            read.target == *target && read.epoch == Some(*epoch),
            "Input target changed"
        );
        self.authorize_terminal_writer(&read, catalog).await?;
        Ok(read)
    }

    pub(crate) async fn handle(
        &self,
        request: Request,
        catalog: Option<&CatalogLane>,
    ) -> Result<Response> {
        match request {
            Request::Health {} => Ok(Response::Health {
                agent_pid: std::process::id(),
                remote_execution_enabled: false,
            }),
            Request::List {} => Ok(Response::Runtimes {
                runtimes: self.list().await?,
            }),
            Request::Create {
                session_id,
                runtime_id,
                cwd,
                argv,
            } => {
                validate_launch(&cwd, &argv)?;
                let runtimes = self.list().await?;
                ensure!(
                    runtimes.len() < RUNTIME_LIMIT,
                    "local spike limit is 16 runtimes"
                );
                ensure!(
                    !runtimes
                        .iter()
                        .any(|r| r.runtime_id == runtime_id || r.session_id == session_id),
                    "session/runtime already exists; reconcile instead of retrying creation"
                );
                self.owners().revoke(runtime_id);
                let cwd = cwd.canonicalize()?;
                let name = tmux_name(session_id, runtime_id);
                let server = self.generation().await?;
                // More than one command argument makes tmux use direct exec, not sh -c.
                let mut args = vec![
                    "new-session",
                    "-d",
                    "-P",
                    "-F",
                    RECEIPT_FORMAT,
                    "-s",
                    &name,
                    "-c",
                    cwd.to_str().context("cwd must be UTF8")?,
                    "-x",
                    "100",
                    "-y",
                    "30",
                    "--",
                    "/usr/bin/env",
                ];
                args.extend(argv.iter().map(String::as_str));
                let receipt = self.command(&args).await?;
                let label = Path::new(&argv[0])
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("CLI");
                let target = format!("={name}:0.0");
                self.command(&["set-option", "-t", &target, "@rs_command", label])
                    .await?;
                let runtime = self.find(runtime_id).await?;
                let fields: Vec<_> = receipt.trim_end().split('|').collect();
                if let Some(identity) = pane_identity(&fields) {
                    let _ = self.stamp_paste(&runtime, server, &identity, None).await;
                }
                Ok(Response::Created { runtime })
            }
            Request::Snapshot { runtime_id } => {
                let runtime = self.find(runtime_id).await?;
                let target = format!("={}:0.0", runtime.tmux_name);
                let text = self
                    .command(&["capture-pane", "-p", "-t", &target, "-S", "-100"])
                    .await?;
                Ok(Response::Snapshot { text })
            }
            Request::Attach { runtime_id } => {
                let runtime = self.find(runtime_id).await?;
                ensure!(
                    !runtime.exited,
                    "runtime has exited; no automatic resurrection"
                );
                Ok(Response::Attachment {
                    tmux_name: runtime.tmux_name,
                })
            }
            Request::Acquire { runtime_id } => {
                self.acquire_terminal(runtime_id, None, catalog).await
            }
            Request::OpenTerminal {
                runtime_id,
                view_id,
            } => {
                ensure!(!view_id.is_nil(), "Invalid terminal view");
                let read = self
                    .geometry_for_runtime(&self.find(runtime_id).await?)
                    .await?;
                self.observe_terminal(&read, catalog).await?;
                let terminal = TerminalView {
                    target: read.target.clone(),
                    agent_epoch: self.agent_epoch,
                    view_id,
                };
                let sample = self.sample_provider(&read).await.ok();
                Ok(Response::TerminalOpened {
                    status: read.status(terminal, sample),
                })
            }
            Request::AcquireTerminal { terminal } => {
                self.acquire_terminal(terminal.target.runtime_id, Some(terminal), catalog)
                    .await
            }
            Request::TerminalStatus { terminal } => {
                if terminal.agent_epoch != self.agent_epoch {
                    return Ok(Response::TerminalChanged {});
                }
                let read = self
                    .geometry(terminal.target.runtime_id, &terminal.target.pane)
                    .await?;
                if read.target != terminal.target || terminal.view_id.is_nil() {
                    return Ok(Response::TerminalChanged {});
                }
                if self.observe_terminal(&read, catalog).await.is_err() {
                    return Ok(Response::TerminalChanged {});
                }
                let sample = self.sample_provider(&read).await.ok();
                Ok(Response::TerminalStatus {
                    status: read.status(terminal, sample),
                })
            }
            Request::ResizeTerminal {
                terminal,
                token,
                sequence,
                size,
            } => {
                self.resize_terminal(terminal, token, sequence, size, catalog)
                    .await
            }
            Request::Renew { runtime_id, token } => {
                let lock = self
                    .existing_lane(runtime_id)
                    .context("Керування втрачено")?;
                let lane = lock.lock().await;
                if lane.pending_paste.is_some() || lane.paste_quarantined {
                    return Ok(control_hold());
                }
                let checked = self
                    .writer_geometry(&lane, runtime_id, token, catalog)
                    .await;
                if let Err(error) = checked {
                    self.owners().release(runtime_id, token);
                    return Err(error);
                }
                let read = checked?;
                if let Some(hold) = read.paste_admission_hold() {
                    self.owners().release(runtime_id, token);
                    return Ok(control_recovering(hold));
                }
                let mut owners = self.owners();
                if let Err(error) = owners.renew(runtime_id, token, self.now()) {
                    owners.release(runtime_id, token);
                    return Err(error);
                }
                Ok(Response::Accepted {})
            }
            Request::Release { runtime_id, token } => {
                let Some(lock) = self.existing_lane(runtime_id) else {
                    return Ok(Response::Accepted {});
                };
                let mut lane = lock.lock().await;
                if let Some(pending) = lane
                    .pending_paste
                    .as_mut()
                    .filter(|pending| pending.binding.token == token)
                {
                    pending.released = true;
                } else {
                    self.owners().release(runtime_id, token);
                }
                Ok(Response::Accepted {})
            }
            Request::PasteStage {
                binding,
                offset,
                data,
            } => Ok(Response::Paste {
                paste: self.paste_stage(binding, offset, data, catalog).await,
            }),
            Request::PasteCommit { binding, bytes } => Ok(Response::Paste {
                paste: self.paste_commit(binding, bytes, catalog).await,
            }),
            Request::PasteAck { binding, receipt } => Ok(Response::Paste {
                paste: self.paste_ack(binding, receipt, catalog).await,
            }),
            Request::Input {
                runtime_id,
                token,
                sequence,
                data,
                paste,
            } => {
                if paste {
                    return Ok(Response::PasteUnavailable {});
                }
                ensure!(
                    (1..=INPUT_CHUNK_LIMIT).contains(&data.len()),
                    "Input must contain 1..4096 bytes"
                );
                let lock = self
                    .existing_lane(runtime_id)
                    .context("Керування втрачено")?;
                let lane = lock.lock().await;
                if lane.pending_paste.is_some() || lane.paste_quarantined {
                    return Ok(control_hold());
                }
                let read = self
                    .writer_geometry(&lane, runtime_id, token, catalog)
                    .await?;
                if let Some(hold) = read.paste_admission_hold() {
                    return Ok(control_recovering(hold));
                }
                ensure!(read.input_enabled, "Terminal input is disabled");
                let guard =
                    format_all([self.paste_guard(&read), format_equal("pane_input_off", 0)]);
                let pane = &read.target.pane;
                let mut branch = format!("send-keys -H -t {pane}");
                for byte in data {
                    use std::fmt::Write;
                    write!(branch, " {byte:02x}").expect("write to string");
                }
                branch.push_str(" ; ");
                branch.push_str(&pane_witness(pane, "rs-input-accepted"));
                {
                    let mut owners = self.owners();
                    owners.input(runtime_id, token, sequence, self.now())?;
                    owners.discard_stage(runtime_id, token);
                }
                let reply = self
                    .command(&["if-shell", "-F", "-t", pane, &guard, &branch])
                    .await?;
                ensure!(
                    reply.trim_end() == "rs-input-accepted",
                    "Input target changed"
                );
                Ok(Response::Accepted {})
            }
            Request::Stop {
                runtime_id,
                expected_pane_id,
            } => {
                let lane = self.lane(runtime_id);
                let _mutation = lane.lock().await;
                let runtime = self.find(runtime_id).await?;
                ensure!(
                    runtime.pane_id == expected_pane_id,
                    "runtime precondition failed"
                );
                let target = format!("={}", runtime.tmux_name);
                self.owners().revoke(runtime_id);
                self.command(&["kill-session", "-t", &target]).await?;
                Ok(Response::Stopped { runtime_id })
            }
        }
    }
}

fn liveness(runtime: &Runtime) -> PaneLiveness {
    if runtime.exited {
        PaneLiveness::Exited
    } else {
        PaneLiveness::Live
    }
}

fn validate_launch(cwd: &Path, argv: &[String]) -> Result<()> {
    ensure!(
        cwd.is_absolute() && cwd.is_dir(),
        "cwd must be an existing absolute directory"
    );
    ensure!(
        !argv.is_empty() && argv.len() <= 64,
        "provide 1..64 command arguments"
    );
    ensure!(
        Path::new(&argv[0]).is_absolute() && Path::new(&argv[0]).is_file(),
        "executable must be an existing absolute file"
    );
    for arg in argv {
        // tmux treats a trailing semicolon as a command separator in its CLI parser.
        if arg.contains('\0') || arg.ends_with(';') || arg.len() > 8192 {
            bail!("argument contains NUL, trailing tmux command separator, or exceeds 8192 bytes");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::w1_resize_tests::Fixture;
    use super::*;

    const DRAFT: &[u8] = "Україна\n日本語\t🙂".as_bytes();
    fn file(f: &Fixture, name: &str) -> Vec<u8> {
        fs::read(f.temp.path().join(name)).unwrap_or_default()
    }

    fn text(f: &Fixture, name: &str) -> String {
        String::from_utf8(file(f, name)).unwrap()
    }

    fn set(f: &Fixture, name: &str, value: &str) {
        fs::write(f.temp.path().join(name), value).unwrap();
    }

    async fn witness(f: &Fixture, name: &str) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while file(f, name).is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("missing pane fixture witness {name}"));
    }

    fn set_clock(engine: &Engine, now: Instant) {
        *engine.paste_test.now.lock().unwrap() = Some(now);
    }

    async fn writer(f: &Fixture) -> PasteBinding {
        f.engine
            .paste_test
            .now
            .lock()
            .unwrap()
            .get_or_insert_with(Instant::now);
        let (token, epoch) = f.acquire(f.terminal.clone()).await;
        PasteBinding {
            terminal: f.terminal.clone(),
            token,
            epoch,
            incarnation: Uuid::new_v4(),
            operation: 1,
            sequence: 0,
        }
    }

    async fn paste_reply(engine: &Engine, request: Request) -> PasteResult {
        match engine.handle(request, None).await.unwrap() {
            Response::Paste { paste } => paste,
            response => panic!("Expected a paste result, got {response:?}"),
        }
    }

    async fn stage(f: &Fixture, binding: &PasteBinding, data: &[u8]) {
        for (chunk, bytes) in data.chunks(INPUT_CHUNK_LIMIT).enumerate() {
            assert_eq!(
                paste_reply(
                    &f.engine,
                    Request::PasteStage {
                        binding: binding.clone(),
                        offset: chunk * INPUT_CHUNK_LIMIT,
                        data: bytes.into(),
                    }
                )
                .await,
                PasteResult::Staged {}
            );
        }
    }

    fn commit(binding: &PasteBinding, bytes: usize) -> Request {
        Request::PasteCommit {
            binding: binding.clone(),
            bytes,
        }
    }

    fn ack(binding: &PasteBinding, receipt: Uuid) -> Request {
        Request::PasteAck {
            binding: binding.clone(),
            receipt,
        }
    }

    fn receipt(result: PasteResult) -> Uuid {
        match result {
            PasteResult::Committed { receipt } => receipt,
            other => panic!("Expected enqueue witness, got {other:?}"),
        }
    }

    fn ordinary(binding: &PasteBinding, sequence: u64) -> Request {
        Request::Input {
            runtime_id: binding.terminal.target.runtime_id,
            token: binding.token,
            sequence,
            data: vec![b'K'],
            paste: false,
        }
    }

    fn release(binding: &PasteBinding) -> Request {
        Request::Release {
            runtime_id: binding.terminal.target.runtime_id,
            token: binding.token,
        }
    }

    fn acquire_requests(terminal: &TerminalView) -> [Request; 2] {
        [
            Request::Acquire {
                runtime_id: terminal.target.runtime_id,
            },
            Request::AcquireTerminal {
                terminal: terminal.clone(),
            },
        ]
    }

    fn spawn_request(
        engine: &Arc<Engine>,
        request: Request,
    ) -> tokio::task::JoinHandle<Result<Response>> {
        let engine = engine.clone();
        tokio::spawn(async move { engine.handle(request, None).await })
    }

    fn spawn_commit(f: &Fixture, binding: &PasteBinding) -> tokio::task::JoinHandle<PasteResult> {
        let engine = f.engine.clone();
        let request = commit(binding, DRAFT.len());
        tokio::spawn(async move { paste_reply(&engine, request).await })
    }

    async fn park_at_admission(
        f: &Fixture,
        binding: &PasteBinding,
    ) -> (Arc<PasteBarrier>, tokio::task::JoinHandle<PasteResult>) {
        stage(f, binding, DRAFT).await;
        let barrier = Arc::new(PasteBarrier::default());
        *f.engine.paste_test.admission.lock().unwrap() = Some(barrier.clone());
        let committed = spawn_commit(f, binding);
        tokio::time::timeout(Duration::from_secs(3), barrier.reached.notified())
            .await
            .unwrap();
        assert!(text(f, "marker").contains("~a~"));
        assert!(file(f, "submissions").is_empty());
        (barrier, committed)
    }

    async fn assert_held(engine: &Engine, binding: &PasteBinding) {
        for request in acquire_requests(&binding.terminal) {
            assert!(matches!(
                engine.handle(request, None).await.unwrap(),
                Response::ControlRecovering {
                    paste_admission: PasteAdmission::PriorSendUnknown
                }
            ));
        }
        assert!(matches!(
            engine
                .handle(ordinary(binding, binding.sequence), None)
                .await,
            Ok(Response::ControlRecovering { .. }) | Err(_)
        ));
        let next = PasteBinding {
            operation: binding.operation + 1,
            ..binding.clone()
        };
        assert_eq!(
            paste_reply(
                engine,
                Request::PasteStage {
                    binding: next.clone(),
                    offset: 0,
                    data: DRAFT.into(),
                }
            )
            .await,
            PasteResult::Unknown {}
        );
        assert!(matches!(
            paste_reply(engine, commit(&next, DRAFT.len())).await,
            PasteResult::Unknown {} | PasteResult::Stale {}
        ));
    }

    #[tokio::test]
    async fn w1_terminal_paste_sender_proof() {
        let f = Fixture::paste();
        let target = &f.terminal.target;
        let read = f
            .engine
            .geometry(target.runtime_id, &target.pane)
            .await
            .unwrap();
        let runtime = f.engine.find(target.runtime_id).await.unwrap();
        let pane = PaneIdentity {
            session: target.session.clone(),
            window: target.window.clone(),
            pane: target.pane.clone(),
            pid: target.pid,
        };
        set(&f, "launch", "");
        set(&f, "marker", "");

        for mode in ["missing-flag", "truncated", ""] {
            set(&f, "probe-mode", mode);
            let stamped = f
                .engine
                .stamp_paste(&runtime, read.server, &pane, None)
                .await;
            let buffer = text(&f, "last-load");
            if mode == "missing-flag" {
                assert!(buffer.is_empty());
            } else {
                assert!(buffer.starts_with("rs-paste-probe-"));
                assert!(!f.temp.path().join(buffer).exists());
            }
            if mode.is_empty() {
                stamped.unwrap();
                let after = f
                    .engine
                    .geometry(target.runtime_id, &target.pane)
                    .await
                    .unwrap();
                assert!(after.paste_ready());
                assert_eq!(after.target, *target);
            } else {
                assert!(stamped.is_err());
                assert!(file(&f, "launch").is_empty());
                assert!(file(&f, "marker").is_empty());
            }
            assert!(file(&f, "submissions").is_empty());
            assert!(file(&f, "pane0-bytes").is_empty());
        }
    }

    #[tokio::test]
    async fn w1_terminal_paste_staged_contract() {
        let f = Fixture::paste();
        let mut b = writer(&f).await;
        let mut expected = Vec::new();
        for bracketed in [false, true] {
            set(&f, "bracketed", if bracketed { "1" } else { "" });
            for data in [
                DRAFT.to_vec(),
                [vec![b'x'; INPUT_CHUNK_LIMIT - 1], DRAFT.into()].concat(),
            ] {
                stage(&f, &b, &data).await;
                let r = receipt(paste_reply(&f.engine, commit(&b, data.len())).await);
                if bracketed {
                    expected.extend_from_slice(b"\x1b[200~");
                }
                expected.extend_from_slice(&data);
                if bracketed {
                    expected.extend_from_slice(b"\x1b[201~");
                }
                assert_eq!(file(&f, "pane0-bytes"), expected);
                assert_eq!(f.engine.owners().input_sequence(&b), Some(b.sequence + 1));
                assert!(text(&f, "marker").contains("~s~"));
                assert!(!f.temp.path().join(text(&f, "last-load")).exists());
                assert!(!text(&f, "last-effect-command").contains("Україна"));
                let held = text(&f, "marker");
                for stale in [
                    PasteBinding {
                        incarnation: Uuid::new_v4(),
                        ..b.clone()
                    },
                    PasteBinding {
                        operation: b.operation + 1,
                        ..b.clone()
                    },
                ] {
                    assert_eq!(
                        paste_reply(&f.engine, ack(&stale, r)).await,
                        PasteResult::Stale {}
                    );
                }
                assert_eq!(
                    paste_reply(&f.engine, ack(&b, Uuid::new_v4())).await,
                    PasteResult::Stale {}
                );
                assert_eq!(text(&f, "marker"), held);
                assert_eq!(
                    paste_reply(&f.engine, ack(&b, r)).await,
                    PasteResult::Acknowledged { writable: true }
                );
                assert!(text(&f, "marker").contains("~r~"));
                assert_ne!(text(&f, "marker"), held);
                b.operation += 1;
                b.sequence += 1;
            }
        }
        set(&f, "last-load", "");
        for invalid in [
            b"\xff".as_slice(),
            b"[draft]\x1b".as_slice(),
            b"\r".as_slice(),
        ] {
            stage(&f, &b, invalid).await;
            assert_eq!(
                paste_reply(&f.engine, commit(&b, invalid.len())).await,
                PasteResult::Invalid {}
            );
            assert_eq!(
                paste_reply(&f.engine, commit(&b, invalid.len())).await,
                PasteResult::Stale {}
            );
            b.operation += 1;
        }
        for bad in [
            (INPUT_CHUNK_LIMIT + 1, vec![b'a']),
            (1, vec![b'a'; INPUT_CHUNK_LIMIT + 1]),
            (1, Vec::new()),
        ] {
            stage(&f, &b, b"x").await;
            assert_eq!(
                paste_reply(
                    &f.engine,
                    Request::PasteStage {
                        binding: b.clone(),
                        offset: bad.0,
                        data: bad.1,
                    }
                )
                .await,
                PasteResult::Invalid {}
            );
            assert_eq!(
                paste_reply(&f.engine, commit(&b, 1)).await,
                PasteResult::Stale {}
            );
            b.operation += 1;
        }
        for (offset, expected) in [(1, PasteResult::Invalid {}), (0, PasteResult::Stale {})] {
            assert_eq!(
                paste_reply(
                    &f.engine,
                    Request::PasteStage {
                        binding: b.clone(),
                        offset,
                        data: DRAFT.into(),
                    }
                )
                .await,
                expected
            );
        }
        assert_eq!(
            paste_reply(&f.engine, commit(&b, DRAFT.len())).await,
            PasteResult::Stale {}
        );
        b.operation += 1;
        stage(&f, &b, &vec![b'x'; PASTE_LIMIT]).await;
        assert_eq!(
            paste_reply(
                &f.engine,
                Request::PasteStage {
                    binding: b.clone(),
                    offset: PASTE_LIMIT,
                    data: vec![b'y'],
                }
            )
            .await,
            PasteResult::Invalid {}
        );
        b.operation += 1;
        set(&f, "unsupported", "1");
        assert_eq!(
            paste_reply(
                &f.engine,
                Request::PasteStage {
                    binding: b.clone(),
                    offset: 0,
                    data: DRAFT.into(),
                }
            )
            .await,
            PasteResult::Unavailable {}
        );
        assert_eq!(
            paste_reply(&f.engine, commit(&b, DRAFT.len())).await,
            PasteResult::Stale {}
        );

        set(&f, "unsupported", "");
        assert_eq!(
            paste_reply(
                &f.engine,
                Request::PasteStage {
                    binding: b.clone(),
                    offset: 0,
                    data: DRAFT.into(),
                }
            )
            .await,
            PasteResult::Stale {}
        );
        assert_eq!(
            paste_reply(&f.engine, commit(&b, DRAFT.len())).await,
            PasteResult::Stale {}
        );
        assert_eq!(f.engine.owners().input_sequence(&b), Some(b.sequence));
        b.operation += 1;
        stage(&f, &b, &DRAFT[..1]).await;
        let mut wrong_target = b.clone();
        wrong_target.terminal.target.process_start += 1;
        assert_eq!(
            paste_reply(
                &f.engine,
                Request::PasteStage {
                    binding: wrong_target,
                    offset: 1,
                    data: DRAFT[1..].into(),
                }
            )
            .await,
            PasteResult::Stale {}
        );
        assert_eq!(
            paste_reply(
                &f.engine,
                Request::PasteStage {
                    binding: b.clone(),
                    offset: 1,
                    data: DRAFT[1..].into(),
                }
            )
            .await,
            PasteResult::Staged {}
        );
        let wrong_sequence = PasteBinding {
            sequence: b.sequence + 1,
            ..b.clone()
        };
        let unrelated = PasteBinding {
            incarnation: Uuid::new_v4(),
            ..wrong_sequence.clone()
        };
        assert_eq!(
            paste_reply(&f.engine, commit(&unrelated, DRAFT.len())).await,
            PasteResult::Stale {}
        );
        assert_eq!(f.engine.owners().input_sequence(&b), Some(b.sequence));
        assert_eq!(
            paste_reply(&f.engine, commit(&wrong_sequence, DRAFT.len())).await,
            PasteResult::Stale {}
        );
        assert_eq!(f.engine.owners().input_sequence(&b), None);
        assert!(f.engine.owners().take_stage(&b).is_none());
        assert!(
            f.engine
                .handle(ordinary(&b, b.sequence), None)
                .await
                .is_err()
        );
        assert!(file(&f, "last-load").is_empty());
        assert_eq!(file(&f, "submissions"), b"1111");
        assert_eq!(file(&f, "effects"), b"1111");
        assert_eq!(file(&f, "pane0-bytes"), expected);
    }

    #[tokio::test]
    async fn w1_terminal_paste_expiry_before_admission() {
        for over in [Duration::ZERO, Duration::from_millis(1)] {
            let f = Fixture::paste();
            let now = Instant::now();
            set_clock(&f.engine, now);
            let b = writer(&f).await;
            let (barrier, task) = park_at_admission(&f, &b).await;
            set_clock(&f.engine, now + INPUT_LEASE_TTL + over);
            barrier.proceed.notify_one();
            assert_eq!(task.await.unwrap(), PasteResult::Stale {});
            assert!(text(&f, "marker").contains("~r~"));
            assert!(file(&f, "submissions").is_empty());
            assert!(file(&f, "pane0-bytes").is_empty());
            assert_eq!(f.engine.owners().input_sequence(&b), Some(0));
            assert!(f.engine.owners().take_stage(&b).is_none());
            assert_eq!(
                paste_reply(&f.engine, commit(&b, DRAFT.len())).await,
                PasteResult::Stale {}
            );
            assert!(matches!(
                f.engine.handle(release(&b), None).await.unwrap(),
                Response::Accepted {}
            ));
            let successor = writer(&f).await;
            assert!(matches!(
                f.engine
                    .handle(ordinary(&successor, 0), None)
                    .await
                    .unwrap(),
                Response::Accepted {}
            ));
            assert_eq!(file(&f, "pane0-bytes"), b"K");
        }
    }

    #[tokio::test]
    async fn w1_terminal_paste_pre_submission_rejection() {
        for (field, value) in [
            ("scope", "wrong".into()),
            (
                "epoch",
                epoch_text(Some(ResizeEpoch {
                    agent: Uuid::new_v4(),
                    owner: Uuid::new_v4(),
                })),
            ),
            ("marker", "wrong nonce".into()),
        ] {
            let f = Fixture::paste();
            let b = writer(&f).await;
            let (barrier, task) = park_at_admission(&f, &b).await;
            set(&f, field, &value);
            barrier.proceed.notify_one();
            assert_eq!(task.await.unwrap(), PasteResult::Unknown {});
            assert!(file(&f, "submissions").is_empty());
            assert!(file(&f, "pane0-bytes").is_empty());
        }
        let f = Fixture::paste();
        let b = writer(&f).await;
        let (barrier, task) = park_at_admission(&f, &b).await;
        set(&f, "input-off", "1");
        barrier.proceed.notify_one();
        assert_eq!(task.await.unwrap(), PasteResult::Unavailable {});
        assert!(file(&f, "submissions").is_empty());
        assert!(file(&f, "pane0-bytes").is_empty());
        assert!(text(&f, "marker").contains("~r~"));
        assert_eq!(f.engine.owners().input_sequence(&b), Some(0));
        assert!(f.engine.owners().take_stage(&b).is_none());
        set(&f, "input-off", "");
        assert!(matches!(
            f.engine.handle(ordinary(&b, 0), None).await.unwrap(),
            Response::Accepted {}
        ));
        assert_eq!(file(&f, "pane0-bytes"), b"K");

        let f = Fixture::paste();
        let b = writer(&f).await;
        stage(&f, &b, DRAFT).await;
        set(&f, "paste-mode", "reject");
        assert_eq!(
            paste_reply(&f.engine, commit(&b, DRAFT.len())).await,
            PasteResult::Unavailable {}
        );
        assert_eq!(file(&f, "submissions"), b"1");
        assert!(file(&f, "effects").is_empty());
        assert!(file(&f, "pane0-bytes").is_empty());
        assert!(matches!(
            f.engine.handle(ordinary(&b, 0), None).await.unwrap(),
            Response::Accepted {}
        ));
        f.engine.handle(release(&b), None).await.unwrap();
        let successor = writer(&f).await;
        assert!(matches!(
            f.engine
                .handle(ordinary(&successor, 0), None)
                .await
                .unwrap(),
            Response::Accepted {}
        ));
        assert_eq!(file(&f, "pane0-bytes"), b"KK");
    }

    #[tokio::test]
    async fn w1_terminal_paste_expiry_in_flight() {
        for (mode, parked, gate) in [
            ("late", "server-parked", "allow-effect"),
            ("hold-reply", "effects", "allow-reply"),
        ] {
            let f = Fixture::paste();
            let now = Instant::now();
            set_clock(&f.engine, now);
            let b = writer(&f).await;
            stage(&f, &b, DRAFT).await;
            set(&f, "paste-mode", mode);
            let task = spawn_commit(&f, &b);
            witness(&f, parked).await;
            assert!(!task.is_finished());
            if mode == "late" {
                assert!(file(&f, "pane0-bytes").is_empty());
            }
            set_clock(&f.engine, now + INPUT_LEASE_TTL + Duration::from_secs(1));
            set(&f, gate, "1");
            let r = receipt(task.await.unwrap());
            assert_eq!(file(&f, "pane0-bytes"), DRAFT);
            assert!(file(&f, "pane1-bytes").is_empty());
            assert_eq!(file(&f, "submissions"), b"1");
            assert_held(&f.engine, &b).await;
            assert_eq!(
                paste_reply(&f.engine, ack(&b, r)).await,
                PasteResult::Acknowledged { writable: false }
            );
            assert!(f.engine.handle(ordinary(&b, 1), None).await.is_err());
            let successor = writer(&f).await;
            assert_ne!(successor.epoch, b.epoch);
        }
    }

    #[tokio::test]
    async fn w1_terminal_paste_handoff_serialization() {
        for mode in ["late", "reject", "timeout-after"] {
            let f = Fixture::paste();
            let now = Instant::now();
            set_clock(&f.engine, now);
            let b = writer(&f).await;
            stage(&f, &b, DRAFT).await;
            set(
                &f,
                "paste-mode",
                if mode == "reject" { "late" } else { mode },
            );
            let task = spawn_commit(&f, &b);
            let parked = if mode == "timeout-after" {
                "effects"
            } else {
                "server-parked"
            };
            witness(&f, parked).await;
            assert_eq!(file(&f, "submissions"), b"1");
            set_clock(&f.engine, now + INPUT_LEASE_TTL + Duration::from_millis(1));
            let first = text(&f, "epoch");
            for request in acquire_requests(&b.terminal) {
                assert!(f.engine.handle(request, None).await.is_err());
            }
            let releasing = spawn_request(&f.engine, release(&b));
            let renewing = spawn_request(
                &f.engine,
                Request::Renew {
                    runtime_id: b.terminal.target.runtime_id,
                    token: b.token,
                },
            );
            let writing = spawn_request(&f.engine, ordinary(&b, 0));
            let pasting = spawn_request(
                &f.engine,
                Request::PasteStage {
                    binding: PasteBinding {
                        operation: 2,
                        ..b.clone()
                    },
                    offset: 0,
                    data: DRAFT.into(),
                },
            );
            tokio::task::yield_now().await;
            assert!(
                !releasing.is_finished()
                    && !renewing.is_finished()
                    && !writing.is_finished()
                    && !pasting.is_finished()
            );
            assert_eq!(text(&f, "epoch"), first);
            if mode == "reject" {
                set(&f, "paste-mode", "reject");
            }
            set(&f, "allow-effect", "1");
            let outcome = task.await.unwrap();
            assert!(matches!(
                releasing.await.unwrap().unwrap(),
                Response::Accepted {}
            ));
            assert!(matches!(
                writing.await.unwrap(),
                Ok(Response::ControlRecovering { .. }) | Err(_)
            ));
            assert!(matches!(
                renewing.await.unwrap(),
                Ok(Response::ControlRecovering { .. }) | Err(_)
            ));
            assert!(matches!(
                pasting.await.unwrap().unwrap(),
                Response::Paste {
                    paste: PasteResult::Unknown {} | PasteResult::Stale {}
                }
            ));
            assert_eq!(text(&f, "epoch"), first);
            if mode == "timeout-after" {
                assert_eq!(outcome, PasteResult::Unknown {});
                assert_held(&f.engine, &b).await;
                assert_eq!(file(&f, "pane0-bytes"), DRAFT);
                continue;
            }
            let expected = if mode == "reject" {
                assert_eq!(outcome, PasteResult::Unavailable {});
                b"K".to_vec()
            } else {
                assert_held(&f.engine, &b).await;
                assert_eq!(
                    paste_reply(&f.engine, ack(&b, receipt(outcome))).await,
                    PasteResult::Acknowledged { writable: false }
                );
                [DRAFT, b"K"].concat()
            };
            let successor = writer(&f).await;
            assert!(matches!(
                f.engine
                    .handle(ordinary(&successor, 0), None)
                    .await
                    .unwrap(),
                Response::Accepted {}
            ));
            assert_eq!(file(&f, "pane0-bytes"), expected);
        }
    }

    #[tokio::test]
    async fn w1_terminal_paste_timeout_quarantine() {
        for mode in ["timeout-before", "timeout-after"] {
            let f = Fixture::with_paste_watchdog(Duration::from_secs(2));
            let b = writer(&f).await;
            stage(&f, &b, DRAFT).await;
            set(&f, "paste-mode", mode);
            let task = spawn_commit(&f, &b);
            let parked = if mode == "timeout-before" {
                "server-parked"
            } else {
                "effects"
            };
            let outcome = tokio::time::timeout(Duration::from_secs(10), async {
                witness(&f, parked).await;
                task.await.unwrap()
            })
            .await
            .unwrap();
            assert_eq!(outcome, PasteResult::Unknown {});
            assert_held(&f.engine, &b).await;
            assert_eq!(file(&f, "submissions"), b"1");
            let held = text(&f, "marker");
            if mode == "timeout-before" {
                assert!(held.contains("~a~"));
                assert!(file(&f, "pane0-bytes").is_empty());
                assert!(f.temp.path().join(text(&f, "last-load")).exists());
                set(&f, "allow-effect", "1");
                witness(&f, "late-result").await;
            } else {
                assert!(held.contains("~s~"));
                set(&f, "allow-reply", "1");
            }
            assert_eq!(file(&f, "pane0-bytes"), DRAFT);
            assert!(text(&f, "marker").contains("~s~"));
            assert_held(&f.engine, &b).await;
            assert_eq!(file(&f, "submissions"), b"1");
        }
    }

    #[tokio::test]
    async fn w1_terminal_paste_restart_quarantine() {
        for (mode, parked) in [("late", "server-parked"), ("hold-reply", "effects")] {
            let f = Fixture::paste();
            let now = Instant::now();
            set_clock(&f.engine, now);
            let b = writer(&f).await;
            stage(&f, &b, DRAFT).await;
            set(&f, "paste-mode", mode);
            let task = spawn_commit(&f, &b);
            witness(&f, parked).await;
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            let (engine, terminal) = f.restart();
            set_clock(&engine, now + INPUT_LEASE_TTL + Duration::from_secs(1));
            let successor = PasteBinding {
                terminal,
                ..b.clone()
            };
            assert!(text(&f, "marker").contains(if mode == "late" { "~a~" } else { "~s~" }));
            let held = text(&f, "marker");
            set(&f, "unsupported", "1");
            assert_held(&engine, &successor).await;
            assert_eq!(text(&f, "marker"), held);
            assert_eq!(file(&f, "submissions"), b"1");
            set(&f, "unsupported", "");
            assert_held(&engine, &successor).await;
            assert!(
                engine
                    .handle(
                        Request::ResizeTerminal {
                            terminal: successor.terminal.clone(),
                            token: successor.token,
                            sequence: 0,
                            size: TerminalSize {
                                cols: 104,
                                rows: 24
                            },
                        },
                        None
                    )
                    .await
                    .is_err()
            );
            engine.handle(release(&successor), None).await.unwrap();
            if mode == "late" {
                set(&f, "allow-effect", "1");
                witness(&f, "late-result").await;
            }
            assert_eq!(file(&f, "pane0-bytes"), DRAFT);
            assert!(file(&f, "pane1-bytes").is_empty());
            assert!(text(&f, "marker").contains("~s~"));
            assert_held(&engine, &successor).await;
        }

        let fresh = Fixture::paste();
        let b = writer(&fresh).await;
        stage(&fresh, &b, DRAFT).await;
        let r = receipt(paste_reply(&fresh.engine, commit(&b, DRAFT.len())).await);
        set(&fresh, "ack-reply", "after");
        assert_eq!(
            paste_reply(&fresh.engine, ack(&b, r)).await,
            PasteResult::Acknowledged { writable: true }
        );
        let (engine, terminal) = fresh.restart();
        let Response::TerminalControl { token, epoch } = engine
            .handle(
                Request::AcquireTerminal {
                    terminal: terminal.clone(),
                },
                None,
            )
            .await
            .unwrap()
        else {
            panic!("acknowledged pane should admit a restart grant");
        };
        let successor = PasteBinding {
            terminal: terminal.clone(),
            token,
            epoch,
            ..b.clone()
        };
        assert!(matches!(
            engine.handle(ordinary(&successor, 0), None).await.unwrap(),
            Response::Accepted {}
        ));
        assert_eq!(file(&fresh, "pane0-bytes"), [DRAFT, b"K"].concat());
        let ready = text(&fresh, "marker");
        for marker in [
            String::new(),
            "v1~bad".into(),
            ready.replace(
                &b.terminal.target.runtime_id.to_string(),
                &Uuid::new_v4().to_string(),
            ),
            ready.replace(&epoch.owner.to_string(), &b.epoch.owner.to_string()),
        ] {
            set(&fresh, "marker", &marker);
            let (engine, terminal) = fresh.restart();
            for request in acquire_requests(&terminal) {
                assert!(matches!(
                    engine.handle(request, None).await.unwrap(),
                    Response::ControlRecovering {
                        paste_admission: PasteAdmission::PriorSendUnknown
                    }
                ));
            }
        }
        let unresolved = Fixture::paste();
        let b = writer(&unresolved).await;
        stage(&unresolved, &b, DRAFT).await;
        let r = receipt(paste_reply(&unresolved.engine, commit(&b, DRAFT.len())).await);
        set(&unresolved, "ack-reply", "before");
        assert_eq!(
            paste_reply(&unresolved.engine, ack(&b, r)).await,
            PasteResult::Unknown {}
        );
        let (engine, terminal) = unresolved.restart();
        assert_held(&engine, &PasteBinding { terminal, ..b }).await;
        assert_eq!(file(&unresolved, "pane0-bytes"), DRAFT);

        let changed = Fixture::paste();
        let b = writer(&changed).await;
        stage(&changed, &b, DRAFT).await;
        let r = receipt(paste_reply(&changed.engine, commit(&b, DRAFT.len())).await);
        let marker = text(&changed, "marker");
        let launch = text(&changed, "launch");
        set(&changed, "launch", &Uuid::new_v4().to_string());
        assert_eq!(
            paste_reply(&changed.engine, ack(&b, r)).await,
            PasteResult::Unknown {}
        );
        assert_eq!(text(&changed, "marker"), marker);
        set(&changed, "launch", &launch);
        let (engine, terminal) = changed.restart();
        assert_held(&engine, &PasteBinding { terminal, ..b }).await;
        assert_eq!(file(&changed, "pane0-bytes"), DRAFT);
        assert_eq!(file(&changed, "submissions"), b"1");
    }

    #[tokio::test]
    async fn w1_terminal_paste_no_replay() {
        let f = Fixture::paste();
        let b = writer(&f).await;
        stage(&f, &b, DRAFT).await;
        let r = receipt(paste_reply(&f.engine, commit(&b, DRAFT.len())).await);
        let args: Vec<String> = serde_json::from_str(&text(&f, "last-effect-command")).unwrap();
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let reply = f.engine.geometry_command(&args).await.unwrap();
        assert_eq!(
            reply.trim(),
            args[5]
                .split_whitespace()
                .last()
                .unwrap()
                .replace("enqueued", "rejected")
        );
        assert_eq!(file(&f, "effects"), b"1");
        assert_eq!(
            paste_reply(&f.engine, ack(&b, r)).await,
            PasteResult::Acknowledged { writable: true }
        );
        assert_eq!(
            paste_reply(&f.engine, commit(&b, DRAFT.len())).await,
            PasteResult::Stale {}
        );
        assert_eq!(
            paste_reply(&f.engine, ack(&b, r)).await,
            PasteResult::Stale {}
        );
        f.engine.handle(release(&b), None).await.unwrap();
        let successor = writer(&f).await;
        assert_eq!(
            paste_reply(&f.engine, commit(&b, DRAFT.len())).await,
            PasteResult::Stale {}
        );
        let mut next = successor.clone();
        next.operation += 1;
        stage(&f, &next, b"other").await;
        set(&f, "paste-mode", "timeout-before");
        assert_eq!(
            paste_reply(&f.engine, commit(&next, 5)).await,
            PasteResult::Unknown {}
        );
        let (engine, terminal) = f.restart();
        let restarted = PasteBinding { terminal, ..next };
        assert_held(&engine, &restarted).await;
        assert_eq!(
            paste_reply(&engine, commit(&restarted, 5)).await,
            PasteResult::Stale {}
        );
        assert_eq!(file(&f, "pane0-bytes"), DRAFT);
        assert!(file(&f, "pane1-bytes").is_empty());
        assert_eq!(file(&f, "submissions"), b"111");
        assert_eq!(file(&f, "effects"), b"1");
    }

    #[test]
    fn launch_validation_rejects_tmux_command_separators() {
        assert!(
            validate_launch(
                Path::new("/tmp"),
                &["/bin/sh".into(), ";".into(), "kill-server".into()]
            )
            .is_err()
        );
        assert!(validate_launch(Path::new("/tmp"), &["sh".into()]).is_err());
        assert!(validate_launch(Path::new("/tmp"), &["/bin/sh".into(), "-i".into()]).is_ok());
    }
}

#[cfg(test)]
#[path = "w1_resize_tests.rs"]
mod w1_resize_tests;
