use crate::{
    catalog::{
        CatalogLane, Operation, OperationStage, PaneIdentity, ServerGeneration, StoredOutcome,
    },
    model::{
        CommittedGeometry, CreateState, LaunchOutcome, ObservationState, PaneLiveness,
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
    ownership: Mutex<crate::ownership::Ownership>,
    lanes: Mutex<HashMap<Uuid, Arc<tokio::sync::Mutex<ResizeLane>>>>,
}

#[derive(Default)]
struct ResizeLane {
    confirmed: Option<(TerminalTarget, ResizeEpoch)>,
    reconcile: bool,
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
}

const EPOCH_OPTION: &str = "@rs_resize_epoch";
const INTENT_OPTION: &str = "@rs_resize_intent";
const GEOMETRY_FORMAT: &str = concat!(
    "#{pid}|#{start_time}|#{@rs_create_generation}|#{session_id}|#{window_id}|#{pane_id}|#{pane_pid}|",
    "#{session_name}|#{window_index}|#{pane_index}|#{@rs_create_nonce}|#{pane_dead}|",
    "#{session_windows}|#{window_panes}|#{window-size}|#{pane_tty}|",
    "#{pane_width}|#{pane_height}|#{window_width}|#{window_height}|",
    "#{@rs_resize_epoch}|#{@rs_resize_intent}"
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

fn unknown(issue: impl Into<Option<ProductIssue>>) -> CreateState {
    CreateState::Unknown {
        issue: issue.into(),
    }
}

fn unavailable() -> ProductIssue {
    ProductIssue::TmuxUnavailable
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

impl GeometryReadback {
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
            ownership: Mutex::default(),
            lanes: Mutex::default(),
        }
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

    async fn command(&self, args: &[&str]) -> Result<String> {
        self.bounded_command(args, Duration::from_secs(3)).await
    }

    async fn geometry_command(&self, args: &[&str]) -> Result<String> {
        self.bounded_command(args, Duration::from_millis(650)).await
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
        let session_target = format!("={}", operation.target);
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
        let nonce = operation.nonce.to_string();
        ensure!(
            lines.next() == Some(&format!("{NONCE_ENV}={nonce}")),
            "missing session launch nonce"
        );
        let row = lines.next().context("missing launch pane")?;
        ensure!(lines.next().is_none(), "multiple candidate panes");
        let fields: Vec<_> = row.split('|').collect();
        ensure!(fields.len() == 14, "invalid launch evidence");
        ensure!(
            generation(&fields[..3].join("|")) == Some(operation.generation),
            "tmux generation changed"
        );
        let (stamped_nonce, stamp) = fields[10].split_once(':').context("missing pane nonce")?;
        let stamp: Vec<_> = stamp.split(':').collect();
        let identity = pane_identity(&stamp).context("invalid launch pane stamp")?;
        ensure!(
            fields[7] == operation.target
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
        Ok(Observation {
            runtime: Runtime {
                session_id: operation.session_id,
                runtime_id: operation.runtime_id,
                tmux_name: operation.target.clone(),
                pane_id: identity.pane.clone(),
                command: operation.profile.executable_name().into(),
                pane_pid,
                exited,
                width: fields[12].parse()?,
                height: fields[13].parse()?,
                origin: RuntimeOrigin::Managed,
                workspace_id: Some(operation.workspace_id),
                operation_id: Some(operation.id),
            },
            identity,
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
        let output = self
            .geometry_command(&["display-message", "-p", "-t", pane, GEOMETRY_FORMAT])
            .await?;
        let mut lines = output.lines();
        let fields: Vec<_> = lines
            .next()
            .context("Missing terminal target")?
            .split('|')
            .collect();
        ensure!(
            lines.next().is_none() && fields.len() == 22,
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
        })
    }

    async fn geometry_for_runtime(
        &self,
        runtime: &Runtime,
        catalog: Option<&CatalogLane>,
    ) -> Result<GeometryReadback> {
        let server = self.generation().await?;
        let read = self.geometry(runtime.runtime_id, &runtime.pane_id).await?;
        ensure!(
            !runtime.exited
                && Some(read.target.pid) == runtime.pane_pid
                && read.server == server
                && read.name == runtime.tmux_name,
            "Terminal identity changed"
        );
        let id = runtime.runtime_id;
        let operations = match catalog {
            Some(lane) => match lane.run(move |db, _| db.by_runtime(&[id])).await {
                Ok(operations) => operations,
                Err(_) if read.nonce.is_empty() => Vec::new(),
                Err(issue) => bail!("Managed terminal catalog unavailable: {issue:?}"),
            },
            None => Vec::new(),
        };
        if let Some(operation) = operations.first() {
            let OperationStage::Finished(StoredOutcome::Launched { identity, .. }) =
                &operation.stage
            else {
                bail!("Managed launch is unconfirmed");
            };
            let observed = self.observe(operation).await?;
            ensure!(
                observed.identity == *identity
                    && read.server == operation.generation
                    && (
                        identity.session.as_str(),
                        identity.window.as_str(),
                        identity.pane.as_str(),
                        identity.pid
                    ) == (
                        read.target.session.as_str(),
                        read.target.window.as_str(),
                        read.target.pane.as_str(),
                        read.target.pid
                    ),
                "Managed terminal identity changed"
            );
        } else {
            ensure!(read.nonce.is_empty(), "Managed terminal is uncorroborated");
        }
        Ok(read)
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
        let (sample, device) = sample_tty(&before.tty)?;
        let after = self
            .geometry(before.target.runtime_id, &before.target.pane)
            .await?;
        ensure!(
            *before == after && tty_identity(&fs::symlink_metadata(&after.tty)?)? == device,
            "Provider sample lost its target identity"
        );
        Ok(sample)
    }

    async fn rotate_epoch(
        &self,
        from: &GeometryReadback,
        successor: ResizeEpoch,
    ) -> Result<GeometryReadback> {
        let conditional_absence = if from.epoch.is_none() { "-o " } else { "" };
        let branch = format!(
            "set-option -p {conditional_absence}-t {} {EPOCH_OPTION} {} ; display-message -p rs-fenced",
            from.target.pane,
            epoch_text(Some(successor))
        );
        let reply = self
            .geometry_command(&[
                "if-shell",
                "-F",
                "-t",
                &from.target.pane,
                &from.guard(from.epoch),
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
            after.target == from.target && after.epoch == Some(successor) && after.tty == from.tty,
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
        self.owners().unowned(runtime_id, Instant::now())?;
        let read = match &terminal {
            Some(terminal) => self.scoped_geometry(terminal).await?,
            None => {
                self.geometry_for_runtime(&self.find(runtime_id).await?, catalog)
                    .await?
            }
        };
        let lane = self.lane(runtime_id);
        let mut lane = lane.try_lock().context("Terminal mutation in progress")?;
        self.owners().unowned(runtime_id, Instant::now())?;
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
            .grant(runtime_id, terminal, Some(epoch), Instant::now())?;
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
                && lane
                    .confirmed
                    .as_ref()
                    .is_some_and(
                        |(target, confirmed)| *target == read.target && *confirmed == epoch
                    ),
            "Resize authority unavailable"
        );
        let revision = read.committed.revision + 1;
        ensure!(revision <= MAX_REVISION, "Terminal revision exhausted");
        self.owners()
            .resize(&terminal, token, epoch, sequence, Instant::now())?;
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
                // More than one command argument makes tmux use direct exec, not sh -c.
                let mut args = vec![
                    "new-session",
                    "-d",
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
                self.command(&args).await?;
                let label = Path::new(&argv[0])
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("CLI");
                let target = format!("={name}:0.0");
                self.command(&["set-option", "-t", &target, "@rs_command", label])
                    .await?;
                Ok(Response::Created {
                    runtime: self.find(runtime_id).await?,
                })
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
                    .geometry_for_runtime(&self.find(runtime_id).await?, catalog)
                    .await?;
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
            } => self.resize_terminal(terminal, token, sequence, size).await,
            Request::Renew { runtime_id, token } => {
                self.owners().renew(runtime_id, token, Instant::now())?;
                Ok(Response::Accepted {})
            }
            Request::Release { runtime_id, token } => {
                self.owners().release(runtime_id, token);
                Ok(Response::Accepted {})
            }
            Request::Input {
                runtime_id,
                token,
                sequence,
                data,
                paste,
            } => {
                ensure!(
                    !data.is_empty() && data.len() <= 4096,
                    "Input must contain 1..4096 bytes"
                );
                let lane = self.lane(runtime_id);
                let _mutation = lane.lock().await;
                let runtime = self.find(runtime_id).await?;
                ensure!(!runtime.exited, "Runtime has exited");
                if let Some(terminal) = self.owners().terminal(runtime_id, token) {
                    ensure!(
                        runtime.pane_id == terminal.target.pane
                            && runtime.pane_pid == Some(terminal.target.pid)
                            && process_start(terminal.target.pid)? == terminal.target.process_start,
                        "Input target changed"
                    );
                }
                self.owners()
                    .input(runtime_id, token, sequence, Instant::now())?;
                // Hex arguments cannot become tmux keys, command separators or shell code.
                let target = format!("={}:0.0", runtime.tmux_name);
                if paste {
                    let buffer = format!("rs-input-{}", Uuid::new_v4().simple());
                    let loaded: Result<()> = tokio::time::timeout(Duration::from_secs(3), async {
                        let mut child = Command::new(&self.tmux)
                            .args(["-N", "-S"])
                            .arg(&self.socket)
                            .args(["load-buffer", "-b", &buffer, "-"])
                            .env_remove("TMUX")
                            .stdin(Stdio::piped())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .kill_on_drop(true)
                            .spawn()?;
                        child
                            .stdin
                            .take()
                            .context("buffer stdin unavailable")?
                            .write_all(&data)
                            .await?;
                        ensure!(child.wait().await?.success(), "paste buffer load failed");
                        self.command(&["paste-buffer", "-p", "-d", "-b", &buffer, "-t", &target])
                            .await?;
                        Ok(())
                    })
                    .await
                    .context("paste timed out; do not retry")
                    .and_then(|result| result);
                    if loaded.is_err() {
                        let _ = self.command(&["delete-buffer", "-b", &buffer]).await;
                    }
                    loaded?;
                    return Ok(Response::Accepted {});
                }
                let hex: Vec<String> = data.iter().map(|byte| format!("{byte:02x}")).collect();
                let mut args = vec!["send-keys", "-H", "-t", &target];
                args.extend(hex.iter().map(String::as_str));
                self.command(&args).await?;
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
    use super::*;
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
