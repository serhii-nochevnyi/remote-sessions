use crate::{
    catalog::{
        CatalogLane, DiagnosticCreate, Operation, OperationStage, PaneIdentity, ServerGeneration,
        StoredOutcome,
    },
    model::{
        CommittedGeometry, CreateState, INPUT_CHUNK_LIMIT, INPUT_LEASE_TTL, LaunchOutcome,
        ObservationState, PASTE_LIMIT, PaneLiveness, PasteAdmission, PasteBinding, PasteResult,
        ProductIssue, ProviderProfile, ProviderSample, Request, ResizeEpoch, ResizeIntent,
        ResizeResult, Response, Runtime, RuntimeOrigin, TerminalSize, TerminalStatus,
        TerminalTarget, TerminalView, parse_name, tmux_name,
    },
    product_launch::{
        LAUNCH_COMMAND, LaunchGuard, NONCE_ENV, TICKET_ENV, read_setup_fault, workspace_locator,
    },
};
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    os::{
        fd::{AsFd, AsRawFd},
        unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};
use uuid::Uuid;

pub struct Engine {
    tmux: PathBuf,
    socket: PathBuf,
    agent_epoch: Uuid,
    ownership: Arc<Mutex<crate::ownership::Ownership>>,
    lanes: Mutex<HashMap<Uuid, Arc<tokio::sync::Mutex<ResizeLane>>>>,
    historical_views: Mutex<HashMap<Uuid, IssuedHistoricalView>>,
    #[cfg(debug_assertions)]
    resize_fixture_target: Option<TerminalTarget>,
    #[cfg(test)]
    paste_test: PasteTest,
}

#[derive(Default)]
struct ResizeLane {
    confirmed: Option<(TerminalTarget, ResizeEpoch)>,
    reconcile: bool,
    pending_paste: Option<PendingPaste>,
    paste_quarantines: Vec<PasteScope>,
}

struct PendingPaste {
    binding: PasteBinding,
    scope: PasteScope,
    receipt: Uuid,
    released: bool,
}

struct PasteScope {
    original: GeometryReadback,
    buffer: Option<PasteBuffer>,
    custody: Option<CustodySlot>,
}

#[derive(Debug, PartialEq, Eq)]
struct PasteBuffer {
    nonce: Uuid,
    operation: u64,
    name: String,
}

impl PasteBuffer {
    fn marker(&self, read: &GeometryReadback, state: char) -> String {
        format!(
            "{}{state}~{}~{}~{}",
            read.paste_prefix(read.epoch),
            self.nonce,
            self.operation,
            self.name
        )
    }

    fn from_marker(read: &GeometryReadback) -> Option<Self> {
        if read.epoch.is_none() || parse_uuid(read.launch()).is_err() {
            return None;
        }
        let marker = read
            .paste_marker
            .strip_prefix(&read.paste_prefix(read.epoch))?;
        let mut fields = marker.split('~');
        if !matches!(fields.next()?, "l" | "a" | "s" | "q" | "f") {
            return None;
        }
        let nonce = parse_uuid(fields.next()?).ok()?;
        let operation = fields.next()?;
        let number: u64 = operation.parse().ok()?;
        let name = fields.next()?;
        let id = name.strip_prefix("rs-paste-")?;
        if fields.next().is_some()
            || number == 0
            || number.to_string() != operation
            || id.len() != 32
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return None;
        }
        Some(Self {
            nonce,
            operation: number,
            name: name.into(),
        })
    }

    fn requires_deletion(&self, read: &GeometryReadback) -> bool {
        read.paste_marker == self.marker(read, 'l') || read.paste_marker == self.marker(read, 'f')
    }
}

struct PayloadCustody {
    runtime: Uuid,
    pane: String,
    name: String,
    tty: String,
    marker: String,
    buffer: PasteBuffer,
    completed: bool,
}

impl PayloadCustody {
    fn matches(&self, read: &GeometryReadback, buffer: &PasteBuffer) -> bool {
        self.name == read.name
            && Path::new(&self.tty) == read.tty
            && self.buffer == *buffer
            && self.marker == buffer.marker(read, 'l')
    }
}

enum CustodyBinding {
    Payload(PayloadCustody),
    Probe(ProbeCustody),
}

impl CustodyBinding {
    fn identity(&self) -> (Uuid, &str, Uuid, &str) {
        match self {
            Self::Payload(p) => (p.runtime, &p.pane, p.buffer.nonce, &p.buffer.name),
            Self::Probe(p) => (p.target.runtime_id, &p.target.pane, p.operation, &p.buffer),
        }
    }

    fn completed(&self) -> bool {
        match self {
            Self::Payload(p) => p.completed,
            Self::Probe(p) => p.completed,
        }
    }

    fn payload(&self) -> Option<&PayloadCustody> {
        match self {
            Self::Payload(p) => Some(p),
            Self::Probe(_) => None,
        }
    }

    fn probe(&self) -> Option<&ProbeCustody> {
        match self {
            Self::Probe(p) => Some(p),
            Self::Payload(_) => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProbeSource {
    Managed {
        operation: Uuid,
        nonce: Uuid,
        ticket: Uuid,
    },
    Diagnostic(Uuid),
    CatalogFree,
}

impl ProbeSource {
    fn parse(source: &str) -> Result<Self> {
        let fields: Vec<_> = source.split(':').collect();
        match fields.as_slice() {
            ["managed", operation, nonce, ticket] => Ok(Self::Managed {
                operation: parse_v4(operation)?,
                nonce: parse_v4(nonce)?,
                ticket: parse_v4(ticket)?,
            }),
            ["diagnostic", nonce] => Ok(Self::Diagnostic(parse_v4(nonce)?)),
            ["local"] => Ok(Self::CatalogFree),
            _ => bail!("Invalid probe custody provenance"),
        }
    }

    fn encode(self) -> String {
        match self {
            Self::Managed {
                operation,
                nonce,
                ticket,
            } => format!("managed:{operation}:{nonce}:{ticket}"),
            Self::Diagnostic(nonce) => format!("diagnostic:{nonce}"),
            Self::CatalogFree => "local".into(),
        }
    }
}

struct ProbeCustody {
    server: ServerGeneration,
    target: TerminalTarget,
    name: String,
    tty: PathBuf,
    device: TtyIdentity,
    source: ProbeSource,
    operation: Uuid,
    buffer: String,
    completed: bool,
}

impl ProbeCustody {
    fn parse(server: ServerGeneration, completed: bool, fields: &[&str]) -> Result<Self> {
        let [
            "probe",
            name,
            token,
            pid,
            start,
            sid,
            wid,
            pane,
            process,
            process_start,
            tty,
            dev,
            ino,
            rdev,
            source,
            operation,
            buffer,
        ] = fields
        else {
            bail!("Malformed probe custody binding");
        };
        let (session, runtime_id) = parse_name(name).context("Invalid probe runtime name")?;
        let session_id = format!("${sid}");
        let window = format!("@{wid}");
        let pane = format!("%{pane}");
        let process = u32::try_from(canonical_number(process)?)?;
        let process_start = canonical_number(process_start)?;
        let operation = parse_v4(operation)?;
        let device = (
            canonical_number(dev)?,
            canonical_number(ino)?,
            canonical_number(rdev)?,
        );
        ensure!(
            !session.is_nil()
                && !runtime_id.is_nil()
                && tmux_name(session, runtime_id) == *name
                && parse_uuid(token)? == server.token
                && canonical_number(pid)? == u64::from(server.pid)
                && canonical_number(start)? == server.start
                && canonical_tmux_id(&session_id, '$')
                && canonical_tmux_id(&window, '@')
                && canonical_tmux_id(&pane, '%')
                && process > 0
                && process_start > 0
                && valid_pane_tty(tty)
                && device.1 > 0
                && *buffer == format!("rs-paste-probe-{}", operation.simple()),
            "Invalid probe custody identity"
        );
        let source = ProbeSource::parse(source)?;
        ensure!(
            !matches!(source, ProbeSource::Managed { operation: create, nonce, ticket }
                if [create, nonce, ticket].contains(&operation))
                && !matches!(source, ProbeSource::Diagnostic(nonce) if operation == nonce),
            "Probe operation duplicates launch provenance"
        );
        Ok(Self {
            server,
            target: TerminalTarget {
                runtime_id,
                generation: server.token,
                session: session_id,
                window,
                pane,
                pid: process,
                process_start,
            },
            name: (*name).into(),
            tty: PathBuf::from(tty),
            device,
            source,
            operation,
            buffer: (*buffer).into(),
            completed,
        })
    }

    fn original_guard(&self, read: &GeometryReadback) -> Result<String> {
        ensure!(
            read.state == PaneState::Live
                && read.target == self.target
                && read.server == self.server
                && read.name == self.name
                && read.tty == self.tty
                && read.epoch.is_none()
                && read.paste_launch.is_empty()
                && read.paste_marker.is_empty()
                && process_start(read.target.pid)? == self.target.process_start
                && tty_identity(&fs::symlink_metadata(&read.tty)?)? == self.device,
            "Original pre-Ready process or TTY is unconfirmed"
        );
        let t = &self.target;
        let tuple = format!("{}:{}:{}:{}", t.session, t.window, t.pane, t.pid);
        let (init, record) = match self.source {
            ProbeSource::Managed {
                operation,
                nonce,
                ticket,
            } => {
                ensure!(
                    read.nonce == format!("{nonce}:{tuple}") && read.diagnostic.is_none(),
                    "Original managed probe stamp changed"
                );
                let pane = PaneIdentity {
                    session: t.session.clone(),
                    window: t.window.clone(),
                    pane: t.pane.clone(),
                    pid: t.pid,
                };
                let init = initializer(
                    &birth_prefix(self.server, operation, t.runtime_id, nonce),
                    ticket,
                    &pane,
                );
                let record = birth_text(&init, self.target.process_start, &self.tty, self.device);
                (init, record)
            }
            ProbeSource::Diagnostic(nonce) => {
                ensure!(
                    read.nonce.is_empty()
                        && read.diagnostic.as_deref() == Some(&format!("{nonce}:{tuple}")),
                    "Original diagnostic probe stamp changed"
                );
                (String::new(), String::new())
            }
            ProbeSource::CatalogFree => {
                ensure!(
                    read.nonce.is_empty() && read.diagnostic.is_none(),
                    "Original catalog-free probe stamp changed"
                );
                (String::new(), String::new())
            }
        };
        Ok(format_all([
            format_equal(DIAGNOSTIC_OPTION, read.diagnostic.as_deref().unwrap_or("")),
            format_equal(INITIALIZER_OPTION, init),
            format_equal(BIRTH_OPTION, record),
        ]))
    }
}

fn parse_v4(value: &str) -> Result<Uuid> {
    let id = parse_uuid(value)?;
    ensure!(
        id.get_version_num() == 4,
        "Expected a version 4 custody nonce"
    );
    Ok(id)
}

struct CustodySlot {
    index: usize,
    value: String,
    mutation: Uuid,
    active: Option<CustodyBinding>,
}

type CustodySnapshot = Box<[Option<CustodySlot>; CUSTODY_SLOTS]>;

#[derive(Debug)]
struct CustodyClaimConflict;

impl std::fmt::Display for CustodyClaimConflict {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Paste custody claim lost its slot")
    }
}

impl std::error::Error for CustodyClaimConflict {}

fn custody_option(server: ServerGeneration, index: usize) -> String {
    format!(
        "@rs_paste_custody_{}_{}_{}_{index:02}",
        server.pid,
        server.start,
        server.token.simple()
    )
}

fn canonical_number(value: &str) -> Result<u64> {
    let number: u64 = value.parse()?;
    ensure!(number.to_string() == value, "Noncanonical custody number");
    Ok(number)
}

impl CustodySlot {
    fn bind(
        read: &GeometryReadback,
        index: usize,
        buffer: &PasteBuffer,
        state: &str,
    ) -> Result<Self> {
        Self::parse(
            read.server,
            index,
            &format!(
                "v1~{}~{state}~{}~{}~{}",
                Uuid::new_v4(),
                read.name,
                read.tty.display(),
                buffer.marker(read, 'l'),
            ),
        )
    }

    fn parse(server: ServerGeneration, index: usize, value: &str) -> Result<Self> {
        ensure!(
            index < CUSTODY_SLOTS
                && value.len() <= CUSTODY_VALUE_LIMIT
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b":/_~-".contains(&byte)),
            "Invalid paste custody value"
        );
        let mut fields = value.split('~');
        let version = fields.next();
        let mutation = parse_v4(fields.next().context("Missing custody mutation")?)?;
        let active = match (version, fields.next()) {
            (Some("v1"), Some("free")) if fields.next().is_none() => None,
            (Some("v2"), Some(state @ ("possible" | "completed"))) => Some(CustodyBinding::Probe(
                ProbeCustody::parse(server, state == "completed", &fields.collect::<Vec<_>>())?,
            )),
            (Some("v1"), Some(state @ ("possible" | "completed"))) => {
                let rest: Vec<_> = fields.collect();
                let [
                    name,
                    tty,
                    "v1",
                    runtime,
                    token,
                    pid,
                    start,
                    sid,
                    wid,
                    pane,
                    process,
                    process_start,
                    create,
                    launch,
                    epoch,
                    "l",
                    nonce,
                    operation,
                    buffer,
                ] = rest.as_slice()
                else {
                    bail!("Malformed paste custody binding");
                };
                let runtime = parse_uuid(runtime)?;
                let (session, named_runtime) =
                    parse_name(name).context("Invalid custody runtime name")?;
                let pane = format!("%{pane}");
                let process = canonical_number(process)?;
                ensure!(
                    !session.is_nil()
                        && named_runtime == runtime
                        && tmux_name(session, runtime) == *name
                        && valid_pane_tty(tty)
                        && parse_uuid(token)? == server.token
                        && canonical_number(pid)? == u64::from(server.pid)
                        && canonical_number(start)? == server.start
                        && canonical_tmux_id(&format!("${sid}"), '$')
                        && canonical_tmux_id(&format!("@{wid}"), '@')
                        && canonical_tmux_id(&pane, '%')
                        && (1..=u64::from(u32::MAX)).contains(&process)
                        && canonical_number(process_start)? > 0
                        && (*create == "-" || parse_uuid(create).is_ok())
                        && parse_uuid(launch).is_ok()
                        && parse_epoch(epoch)?.is_some(),
                    "Invalid paste custody identity"
                );
                let nonce = parse_uuid(nonce)?;
                let operation = canonical_number(operation)?;
                let id = buffer
                    .strip_prefix("rs-paste-")
                    .context("Invalid custody buffer name")?;
                let uuid = Uuid::try_parse(id)?;
                ensure!(
                    nonce.get_version_num() == 4
                        && operation > 0
                        && uuid.get_version_num() == 4
                        && uuid.simple().to_string() == id,
                    "Invalid paste custody buffer"
                );
                Some(CustodyBinding::Payload(PayloadCustody {
                    runtime,
                    pane,
                    name: (*name).into(),
                    tty: (*tty).into(),
                    marker: rest[2..].join("~"),
                    buffer: PasteBuffer {
                        nonce,
                        operation,
                        name: (*buffer).into(),
                    },
                    completed: state == "completed",
                }))
            }
            _ => bail!("Invalid paste custody version or state"),
        };
        Ok(Self {
            index,
            value: value.into(),
            mutation,
            active,
        })
    }

    fn bind_probe(read: &GeometryReadback, index: usize, source: ProbeSource) -> Result<Self> {
        let t = &read.target;
        let (dev, ino, rdev) = tty_identity(&fs::symlink_metadata(&read.tty)?)?;
        let operation = Uuid::new_v4();
        let buffer = format!("rs-paste-probe-{}", operation.simple());
        Self::parse(
            read.server,
            index,
            &format!(
                "v2~{}~possible~probe~{}~{}~{}~{}~{}~{}~{}~{}~{}~{}~{dev}~{ino}~{rdev}~{}~{operation}~{buffer}",
                Uuid::new_v4(),
                read.name,
                t.generation,
                read.server.pid,
                read.server.start,
                &t.session[1..],
                &t.window[1..],
                &t.pane[1..],
                t.pid,
                t.process_start,
                read.tty.display(),
                source.encode(),
            ),
        )
    }

    fn complete_probe(&self, server: ServerGeneration) -> Result<Self> {
        let (header, binding) = self
            .value
            .split_once("~possible~")
            .context("Missing possible probe custody")?;
        ensure!(
            header == format!("v2~{}", self.mutation),
            "Invalid possible probe custody"
        );
        Self::parse(
            server,
            self.index,
            &format!("v2~{}~completed~{binding}", Uuid::new_v4()),
        )
    }

    fn guard(&self, server: ServerGeneration) -> String {
        format_equal(&custody_option(server, self.index), &self.value)
    }
}

fn free_custody_index(snapshot: &CustodySnapshot) -> Option<usize> {
    snapshot
        .iter()
        .position(|slot| slot.as_ref().is_some_and(|slot| slot.active.is_none()))
        .or_else(|| snapshot.iter().position(Option::is_none))
}

fn paste_process_stamp(marker: &str) -> Result<(&str, u64)> {
    let mut fields = marker.split('~').skip(8);
    let pid = fields.next().context("Missing original paste process ID")?;
    let started = fields
        .next()
        .context("Missing original paste process start")?;
    let value: u64 = started.parse()?;
    ensure!(
        value > 0 && value.to_string() == started,
        "Invalid original paste process start"
    );
    Ok((pid, value))
}

impl PasteScope {
    fn sampled(read: &GeometryReadback) -> Self {
        Self {
            original: read.clone(),
            buffer: PasteBuffer::from_marker(read),
            custody: None,
        }
    }

    fn may_be_same_pane(&self, read: &GeometryReadback) -> bool {
        self.original.same_pane(read)
            || (self.original.server.pid == read.server.pid
                && self.original.server.start == read.server.start
                && self.original.target.pane == read.target.pane)
    }
}

impl ResizeLane {
    fn holds_pane(&self, read: &GeometryReadback) -> bool {
        self.pending_paste
            .as_ref()
            .is_some_and(|pending| pending.scope.may_be_same_pane(read))
            || self
                .paste_quarantines
                .iter()
                .any(|scope| scope.may_be_same_pane(read))
    }

    fn has_paste_hold(&self) -> bool {
        self.pending_paste.is_some() || !self.paste_quarantines.is_empty()
    }

    fn retain_paste(&mut self, scope: PasteScope) {
        if let Some(previous) = self
            .paste_quarantines
            .iter_mut()
            .find(|previous| previous.original.same_pane(&scope.original))
        {
            if previous.buffer.is_none()
                && previous.original.epoch == scope.original.epoch
                && previous.original.paste_marker == scope.original.paste_marker
            {
                previous.buffer = scope.buffer;
                previous.custody = scope.custody;
            }
        } else {
            self.paste_quarantines.push(scope);
        }
    }
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
    custody_claim: Mutex<Option<Arc<PasteBarrier>>>,
    admission: Mutex<Option<Arc<PasteBarrier>>>,
    sender_fixture: bool,
    provider_sample: Option<ProviderSample>,
    geometry_watchdog: Option<Duration>,
    send_watchdog: Option<Duration>,
    launch_program: Option<PathBuf>,
    replacement_start: Option<u64>,
}

#[cfg(test)]
#[derive(Default)]
struct PasteBarrier {
    reached: tokio::sync::Notify,
    proceed: tokio::sync::Notify,
}

#[cfg(test)]
impl PasteBarrier {
    async fn pause_once(slot: &Mutex<Option<Arc<Self>>>) {
        let barrier = slot.lock().unwrap().take();
        if let Some(barrier) = barrier {
            barrier.reached.notify_one();
            barrier.proceed.notified().await;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GeometryReadback {
    target: TerminalTarget,
    state: PaneState,
    server: ServerGeneration,
    name: String,
    nonce: String,
    diagnostic: Option<String>,
    tty: PathBuf,
    epoch: Option<ResizeEpoch>,
    committed: CommittedGeometry,
    paste_launch: String,
    paste_marker: String,
    sender_supported: bool,
    input_enabled: bool,
}

#[derive(Clone, Copy)]
struct GeometryIdentity<'a> {
    target: &'a TerminalTarget,
    state: PaneState,
    server: ServerGeneration,
    name: &'a str,
    nonce: &'a str,
    diagnostic: Option<&'a str>,
    tty: &'a Path,
    sample: Option<&'a GeometryReadback>,
}

struct RuntimeCatalog {
    managed: Option<Operation>,
    diagnostic: Option<DiagnosticCreate>,
}

enum TerminalStatusFailure {
    Changed,
    Unavailable(anyhow::Error),
}

#[derive(Debug)]
struct UnprovenTerminalBirth;

impl std::fmt::Display for UnprovenTerminalBirth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Original terminal birth is unconfirmed")
    }
}

impl From<anyhow::Error> for TerminalStatusFailure {
    fn from(error: anyhow::Error) -> Self {
        Self::Unavailable(error)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneState {
    Live,
    Exited(Option<u32>),
}

const EPOCH_OPTION: &str = "@rs_resize_epoch";
const INTENT_OPTION: &str = "@rs_resize_intent";
const PASTE_LAUNCH_OPTION: &str = "@rs_paste_launch";
const PASTE_OPTION: &str = "@rs_paste_admission";
const PASTE_WATCHDOG: Duration = Duration::from_millis(650);
const GEOMETRY_FORMAT: &str = concat!(
    "#{pid}|#{start_time}|#{@rs_create_generation}|#{session_id}|#{window_id}|#{pane_id}|#{pane_pid}|",
    "#{session_name}|#{window_index}|#{pane_index}|#{@rs_create_nonce}",
    "#{?@rs_diagnostic_create,~#{@rs_diagnostic_create},}|#{pane_dead}|",
    "#{session_windows}|#{window_panes}|#{window-size}|#{pane_tty}|",
    "#{pane_width}|#{pane_height}|#{window_width}|#{window_height}|",
    "#{@rs_resize_epoch}|#{@rs_resize_intent}|",
    "#{?#{m/r:^[0-9a-f-]*$,#{@rs_paste_launch}},#{@rs_paste_launch},invalid}",
    "#{?#{m/r:^..............8,#{@rs_paste_launch}},~#{@rs_create_birth},}|",
    "#{?#{m/r:^[a-z0-9:~-]*$,#{@rs_paste_admission}},#{@rs_paste_admission},invalid}|",
    "#{pane_input_off}"
);
const MAX_REVISION: u64 = (1 << 53) - 1;
const GENERATION_OPTION: &str = "@rs_create_generation";
const GENERATION_FORMAT: &str = "#{pid}|#{start_time}|#{@rs_create_generation}";
const CUSTODY_SLOTS: usize = 16;
const CUSTODY_VALUE_LIMIT: usize = 2048;
const CUSTODY_SNAPSHOT_LIMIT: usize = 40960;
const CUSTODY_ROW: &str =
    "#{option_name} #{?#{m/r:^[a-zA-Z0-9:/_~-]+$,#{option_value}},#{option_value},invalid}";
const CUSTODY_END: &str = "rs-custody-snapshot";
const PANE_OPTION: &str = "@rs_create_nonce";
const DIAGNOSTIC_OPTION: &str = "@rs_diagnostic_create";
const INITIALIZER_OPTION: &str = "@rs_create_initializer";
const BIRTH_OPTION: &str = "@rs_create_birth";
const BIRTH_FORMAT: &str = concat!(
    "#{pid}|#{start_time}|#{@rs_create_generation}|",
    "#{session_id}|#{window_id}|#{pane_id}|#{pane_pid}|",
    "#{session_name}|#{window_index}|#{pane_index}|#{@rs_create_nonce}|#{pane_dead}|",
    "#{pane_tty}|#{@rs_create_initializer}|#{@rs_create_birth}"
);
const BIRTH_RETRY: Duration = Duration::from_millis(25);
const BIRTH_READ_BUDGET: Duration = Duration::from_millis(300);
const RECEIPT_FORMAT: &str = "#{session_id}|#{window_id}|#{pane_id}|#{pane_pid}";
const EVIDENCE_FORMAT: &str = concat!(
    "#{pid}|#{start_time}|#{@rs_create_generation}|",
    "#{session_id}|#{window_id}|#{pane_id}|#{pane_pid}|",
    "#{session_name}|#{window_index}|#{pane_index}|#{@rs_create_nonce}|",
    "#{pane_dead}|#{pane_width}|#{pane_height}"
);
const INVENTORY_PANE_FORMAT: &str = concat!(
    "#{pid}|#{start_time}|#{@rs_create_generation}|",
    "#{session_id}|#{window_id}|#{pane_id}|#{pane_pid}|",
    "#{session_name}|#{pane_dead}|#{@rs_create_nonce}"
);
const LIST_FORMAT: &str = concat!(
    "#{session_name}\t#{pane_id}\t#{pane_pid}\t#{pane_dead}\t#{pane_width}\t#{pane_height}\t",
    "#{?@rs_command,#{@rs_command},#{pane_current_command}}"
);
const INVENTORY_SCOPE_FORMAT: &str = concat!(
    "#{pid}|#{start_time}|",
    "#{?#{m/r:^[0-9a-f-]*$,#{@rs_create_generation}},#{@rs_create_generation},invalid}|",
    "#{session_id}|#{window_id}"
);
const RUNTIME_LIMIT: usize = 16;
const INVENTORY_BUDGET: Duration = Duration::from_millis(4950);
const INVENTORY_PROOF_MIN_BUDGET: Duration = Duration::from_millis(150);

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct LaunchBirth {
    pub tmux: PathBuf,
    pub server: ServerGeneration,
    pub operation: Uuid,
    pub session: Uuid,
    pub runtime: Uuid,
    pub ticket: Uuid,
}

fn birth_prefix(server: ServerGeneration, operation: Uuid, runtime: Uuid, nonce: Uuid) -> String {
    format!(
        "v1~{}~{}~{}~{operation}~{runtime}~{nonce}~",
        server.pid, server.start, server.token
    )
}

fn initializer(prefix: &str, ticket: Uuid, pane: &PaneIdentity) -> String {
    format!(
        "{prefix}{ticket}~{}~{}~{}~{}",
        pane.session, pane.window, pane.pane, pane.pid
    )
}

fn birth_text(init: &str, start: u64, tty: &Path, (dev, ino, rdev): TtyIdentity) -> String {
    format!("{init}~{start}~{}~{dev}~{ino}~{rdev}", tty.display())
}

fn birth_launch_id(ticket: Uuid) -> Uuid {
    let mut bytes = *ticket.as_bytes();
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    Uuid::from_bytes(bytes)
}

fn generation_guard(server: ServerGeneration) -> String {
    format_all([
        format_equal("pid", server.pid),
        format_equal("start_time", server.start),
        format_equal(GENERATION_OPTION, server.token),
    ])
}

fn creation_guard(server: ServerGeneration) -> String {
    format_all([
        generation_guard(server),
        format_equal("after-if-shell", ""),
        format_equal("after-new-session", ""),
        format_equal("session-created", ""),
        format_equal("session-window-changed", ""),
        format_equal("window-linked", ""),
        format_equal("window-layout-changed", ""),
        format_equal("after-set-environment", ""),
        format_equal("after-set-option", ""),
        format_equal("command-error", ""),
    ])
}

impl LaunchBirth {
    pub(crate) async fn publish(&self, directory: &Path, nonce: Uuid) -> Result<()> {
        let engine = Engine::new(self.tmux.clone(), directory.join("tmux.sock"));
        ensure!(
            engine.private_sender_socket(),
            "Launch socket is unavailable"
        );
        let pane = std::env::var("TMUX_PANE")?;
        ensure!(canonical_tmux_id(&pane, '%'), "Invalid launch pane");
        let pid = std::process::id();
        let start = process_start(pid)?;
        let mut tty = [0; 128];
        let result = unsafe {
            nix::libc::ttyname_r(std::io::stdin().as_raw_fd(), tty.as_mut_ptr(), tty.len())
        };
        ensure!(result == 0, "Launch terminal is unavailable");
        let tty = unsafe { std::ffi::CStr::from_ptr(tty.as_ptr()) }.to_str()?;
        ensure!(valid_pane_tty(tty), "Unsupported launch terminal");
        let terminal = fs::File::from(std::io::stdin().as_fd().try_clone_to_owned()?);
        let device = tty_identity(&terminal.metadata()?)?;
        ensure!(
            tty_identity(&fs::symlink_metadata(tty)?)? == device,
            "Launch terminal changed"
        );
        let row = engine
            .geometry_command(&["display-message", "-p", "-t", &pane, RECEIPT_FORMAT])
            .await?;
        let fields: Vec<_> = row.trim_end().split('|').collect();
        let identity = pane_identity(&fields).context("Invalid launch pane identity")?;
        ensure!(
            identity.pane == pane && identity.pid == pid,
            "Launch process changed"
        );
        let name = tmux_name(self.session, self.runtime);
        let stamp = format!(
            "{nonce}:{}:{}:{pane}:{pid}",
            identity.session, identity.window
        );
        let init = initializer(
            &birth_prefix(self.server, self.operation, self.runtime, nonce),
            self.ticket,
            &identity,
        );
        let birth = birth_text(&init, start, Path::new(tty), device);
        let guard = format_all([
            generation_guard(self.server),
            engine.sender_guard(),
            format_equal("session_name", name),
            format_equal("session_id", &identity.session),
            format_equal("window_id", &identity.window),
            format_equal("pane_id", &pane),
            format_equal("pane_pid", pid),
            format_equal("pane_tty", tty),
            format_equal("pane_dead", 0),
            format_equal(PANE_OPTION, stamp),
            format_equal(INITIALIZER_OPTION, &init),
            format_equal(BIRTH_OPTION, ""),
            format_equal("after-if-shell", ""),
        ]);
        let branch = format!("set-option -po -t {pane} {BIRTH_OPTION} '{birth}'");
        engine
            .geometry_command(&["if-shell", "-F", "-t", &pane, &guard, &branch])
            .await?;
        Ok(())
    }
}

struct Observation {
    identity: PaneIdentity,
    runtime: Runtime,
}

enum ManagedProvenance {
    Birth(OriginalBirth),
    Historical(HistoricalProof),
}

#[derive(Clone, PartialEq, Eq)]
struct HistoricalOrigin {
    server: ServerGeneration,
    name: String,
    nonce: String,
    launch: Uuid,
    tty: PathBuf,
    device: TtyIdentity,
}

struct HistoricalProof {
    origin: HistoricalOrigin,
    marker: String,
    ready: bool,
}

struct IssuedHistoricalView {
    terminal: TerminalView,
    origin: HistoricalOrigin,
    expires: Instant,
}

const HISTORICAL_VIEW_LIMIT: usize = 64;
const HISTORICAL_VIEW_TTL: Duration = Duration::from_secs(300);

impl ManagedProvenance {
    fn tty(&self) -> &Path {
        match self {
            Self::Birth(proof) => &proof.tty,
            Self::Historical(proof) => &proof.origin.tty,
        }
    }

    fn guard(&self) -> String {
        match self {
            Self::Birth(proof) => proof.guard(),
            Self::Historical(proof) => format_all([
                format_equal(INITIALIZER_OPTION, ""),
                format_equal(BIRTH_OPTION, ""),
                format_equal(PASTE_LAUNCH_OPTION, proof.origin.launch),
                format_equal(PASTE_OPTION, &proof.marker),
            ]),
        }
    }
}

struct OriginalBirth {
    initializer: String,
    record: String,
    ticket: Uuid,
    start: u64,
    tty: PathBuf,
    device: TtyIdentity,
    used: bool,
}

impl OriginalBirth {
    fn guard(&self) -> String {
        let record = if self.used {
            format!("used~{}", self.record)
        } else {
            self.record.clone()
        };
        format_all([
            format_equal(INITIALIZER_OPTION, &self.initializer),
            format_equal(BIRTH_OPTION, record),
        ])
    }
}

#[derive(Clone, Copy)]
struct BirthTarget<'a> {
    name: &'a str,
    server: ServerGeneration,
    nonce: Uuid,
}

#[derive(PartialEq, Eq)]
struct BirthSample {
    tty: PathBuf,
    initializer: String,
    record: String,
}

enum GenerationSample {
    Stamped(ServerGeneration),
    Unstamped { pid: u32, start: u64 },
}

impl GenerationSample {
    fn candidate(self) -> ServerGeneration {
        match self {
            Self::Stamped(server) => server,
            Self::Unstamped { pid, start } => ServerGeneration {
                pid,
                start,
                token: Uuid::new_v4(),
            },
        }
    }
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
    let token = parse_uuid(token).ok()?;
    Some(ServerGeneration { pid, start, token })
}

fn canonical_tmux_id(value: &str, prefix: char) -> bool {
    value.strip_prefix(prefix).is_some_and(|digits| {
        digits
            .parse::<u32>()
            .is_ok_and(|id| id.to_string() == digits)
    })
}

fn pane_identity(fields: &[&str]) -> Option<PaneIdentity> {
    let [session, window, pane, pid] = fields else {
        return None;
    };
    for (value, prefix) in [(session, '$'), (window, '@'), (pane, '%')] {
        if !canonical_tmux_id(value, prefix) {
            return None;
        }
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
    fn identity(&self) -> GeometryIdentity<'_> {
        GeometryIdentity {
            target: &self.target,
            state: self.state,
            server: self.server,
            name: &self.name,
            nonce: &self.nonce,
            diagnostic: self.diagnostic.as_deref(),
            tty: &self.tty,
            sample: Some(self),
        }
    }

    fn same_pane(&self, other: &Self) -> bool {
        self.target == other.target
            && self.server == other.server
            && self.name == other.name
            && self.nonce == other.nonce
            && self.diagnostic == other.diagnostic
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
            self.launch(),
            epoch_text(epoch),
        )
    }

    fn ready_marker(&self, epoch: Option<ResizeEpoch>) -> String {
        format!("{}r~{}", self.paste_prefix(epoch), Uuid::new_v4())
    }

    fn paste_ready(&self) -> bool {
        parse_uuid(self.launch()).is_ok()
            && self.birth_ready()
            && self
                .paste_marker
                .strip_prefix(&format!("{}r~", self.paste_prefix(self.epoch)))
                .is_some_and(|nonce| parse_uuid(nonce).is_ok())
    }

    fn launch(&self) -> &str {
        self.paste_launch
            .split_once('~')
            .map_or(&self.paste_launch, |(launch, _)| launch)
    }

    fn birth(&self) -> Option<&str> {
        self.paste_launch.split_once('~').map(|(_, birth)| birth)
    }

    fn historical_launch(&self) -> Option<Uuid> {
        parse_uuid(&self.paste_launch).ok().filter(|launch| {
            launch.get_version_num() == 4 && launch.get_variant() == uuid::Variant::RFC4122
        })
    }

    fn birth_bound(&self) -> bool {
        parse_uuid(self.launch()).is_ok_and(|id| id.get_version_num() == 8)
    }

    fn birth_ready(&self) -> bool {
        if !self.birth_bound() {
            return true;
        }
        let t = &self.target;
        let Some((nonce, _)) = self.nonce.split_once(':') else {
            return false;
        };
        let Some(birth) = self.birth() else {
            return false;
        };
        let mut fields = birth.split('~');
        let (Some("used"), Some(operation), Some(ticket)) =
            (fields.next(), fields.nth(4), fields.nth(2))
        else {
            return false;
        };
        let (Ok(operation), Ok(ticket), Ok(nonce)) =
            (parse_uuid(operation), parse_uuid(ticket), parse_uuid(nonce))
        else {
            return false;
        };
        let mut device = birth.rsplit('~').take(3).map(str::parse::<u64>);
        let (Some(Ok(rdev)), Some(Ok(ino)), Some(Ok(dev))) =
            (device.next(), device.next(), device.next())
        else {
            return false;
        };
        let device = (dev, ino, rdev);
        if self.state == PaneState::Live
            && !fs::symlink_metadata(&self.tty)
                .is_ok_and(|meta| tty_identity(&meta).is_ok_and(|actual| actual == device))
        {
            return false;
        }
        let pane = PaneIdentity {
            session: t.session.clone(),
            window: t.window.clone(),
            pane: t.pane.clone(),
            pid: t.pid,
        };
        let prefix = birth_prefix(self.server, operation, t.runtime_id, nonce);
        let init = initializer(&prefix, ticket, &pane);
        ticket.get_version_num() == 4
            && birth_launch_id(ticket).to_string() == self.launch()
            && birth
                == format!(
                    "used~{}",
                    birth_text(&init, t.process_start, &self.tty, device)
                )
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

    fn writer_refusal(&self, error: anyhow::Error) -> Result<Response> {
        if error.is::<UnprovenTerminalBirth>()
            && self.birth_bound()
            && let Some(hold) = self.paste_admission_hold()
        {
            return Ok(control_recovering(hold));
        }
        Err(error)
    }

    fn guard(&self, predecessor: Option<ResizeEpoch>) -> String {
        self.guard_with_generation(predecessor, &self.target.generation.to_string())
    }

    fn guard_with_generation(&self, predecessor: Option<ResizeEpoch>, token: &str) -> String {
        let (dead, pid) = match self.state {
            PaneState::Live => (0, Some(self.target.pid)),
            PaneState::Exited(pid) => (1, pid),
        };
        let mut predicates = vec![
            format_equal("pid", self.server.pid),
            format_equal("start_time", self.server.start),
            format_equal(GENERATION_OPTION, token),
            format_equal("session_name", &self.name),
            format_equal("session_id", &self.target.session),
            format_equal("window_id", &self.target.window),
            format_equal("pane_id", &self.target.pane),
            format_equal("window_index", 0),
            format_equal("pane_index", 0),
            format_equal(
                "pane_pid",
                pid.map_or_else(String::new, |pid| pid.to_string()),
            ),
            format_equal("pane_tty", self.tty.display()),
            format_equal(PANE_OPTION, &self.nonce),
            format_equal("session_windows", 1),
            format_equal("window_panes", 1),
            format_equal("pane_dead", dead),
            format_equal("window-size", "manual"),
            format_equal("after-set-option", ""),
            format_equal("after-resize-window", ""),
            format_equal("after-display-message", ""),
            format_equal("command-error", ""),
            format_equal(EPOCH_OPTION, epoch_text(predecessor)),
        ];
        if self.birth_bound() {
            predicates.push(format_equal(BIRTH_OPTION, self.birth().unwrap_or("")));
        } else if !self.nonce.is_empty() && self.historical_launch().is_some() {
            predicates.push(format_equal(INITIALIZER_OPTION, ""));
            predicates.push(format_equal(BIRTH_OPTION, ""));
        }
        if let Some(stamp) = &self.diagnostic {
            predicates.push(format_equal(DIAGNOSTIC_OPTION, stamp));
        }
        format_all(predicates)
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

type TtyIdentity = (u64, u64, u64);

fn tty_identity(metadata: &fs::Metadata) -> Result<TtyIdentity> {
    ensure!(
        metadata.file_type().is_char_device() && metadata.uid() == nix::unistd::geteuid().as_raw(),
        "Unexpected provider terminal device"
    );
    Ok((metadata.dev(), metadata.ino(), metadata.rdev()))
}

fn valid_pane_tty(tty: &str) -> bool {
    tty.strip_prefix("/dev/ttys")
        .or_else(|| tty.strip_prefix("/dev/pts/"))
        .is_some_and(|suffix| {
            !suffix.is_empty() && suffix.bytes().all(|c| c.is_ascii_alphanumeric())
        })
}

#[cfg(any(debug_assertions, test))]
#[doc(hidden)]
pub fn launch_fixture_process_start(pid: u32) -> Result<u64> {
    process_start(pid)
}

fn sample_tty(path: &Path) -> Result<(ProviderSample, TtyIdentity)> {
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
            historical_views: Mutex::default(),
            #[cfg(debug_assertions)]
            resize_fixture_target: None,
            #[cfg(test)]
            paste_test: PasteTest::default(),
        }
    }

    #[cfg(debug_assertions)]
    pub(crate) async fn for_resize_fixture(mut self, target: TerminalTarget) -> Result<Self> {
        ensure!(
            self.private_sender_socket(),
            "Resize fixture requires a private real tmux socket"
        );
        let read = self.geometry(target.runtime_id, &target.pane).await?;
        ensure!(
            read.target == target && read.sender_supported,
            "Resize fixture target or sender changed"
        );
        self.resize_fixture_target = Some(target);
        Ok(self)
    }

    #[cfg(debug_assertions)]
    pub(crate) fn resize_fixture_target(&self) -> Option<&TerminalTarget> {
        self.resize_fixture_target.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn with_fixture_sender(mut self) -> Self {
        self.paste_test.sender_fixture = true;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_creation_fixture(
        mut self,
        launch_program: Option<PathBuf>,
        replacement_start: Option<u64>,
    ) -> Self {
        self.paste_test.launch_program = launch_program;
        self.paste_test.replacement_start = replacement_start;
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
            format_equal(PASTE_LAUNCH_OPTION, read.launch()),
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
        if let Some(watchdog) = self.paste_test.geometry_watchdog.or_else(|| {
            self.paste_test
                .sender_fixture
                .then_some(Duration::from_secs(3))
        }) {
            return watchdog;
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
        Ok(self
            .list_runtimes(false, None)
            .await?
            .into_iter()
            .map(|(runtime, _)| runtime)
            .collect())
    }

    async fn list_runtimes(
        &self,
        with_scope: bool,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<Vec<(Runtime, Option<String>)>> {
        let remaining = || {
            deadline.map_or(Duration::from_secs(3), |end| {
                end.saturating_duration_since(tokio::time::Instant::now())
            })
        };
        // list-panes needs a target even with -a when the live server is empty.
        // list-sessions distinguishes that valid state from an unavailable server.
        if self
            .bounded_command(&["list-sessions", "-F", "#{session_name}"], remaining())
            .await?
            .is_empty()
        {
            return Ok(Vec::new());
        }
        let scoped_format = with_scope.then(|| format!("{LIST_FORMAT}\t{INVENTORY_SCOPE_FORMAT}"));
        let format = scoped_format.as_deref().unwrap_or(LIST_FORMAT);
        let output = self
            .bounded_command(&["list-panes", "-a", "-F", format], remaining())
            .await?;
        let mut runtimes: Vec<(Runtime, Option<String>)> = Vec::new();
        for line in output.lines() {
            let fields: Vec<_> = line.split('\t').collect();
            ensure!(
                fields.len() == 7 || (with_scope && fields.len() == 8),
                "unexpected tmux metadata"
            );
            let Some((session_id, runtime_id)) = parse_name(fields[0]) else {
                continue;
            };
            ensure!(canonical_tmux_id(fields[1], '%'), "Invalid managed pane ID");
            ensure!(
                !runtimes
                    .iter()
                    .any(|(runtime, _)| runtime.runtime_id == runtime_id),
                "multiple panes for one runtime are unsupported"
            );
            let runtime = Runtime {
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
            };
            runtimes.push((runtime, fields.get(7).map(|scope| (*scope).to_owned())));
        }
        Ok(runtimes)
    }

    async fn sample_generation(&self) -> Result<GenerationSample> {
        let output = self
            .command(&["display-message", "-p", GENERATION_FORMAT])
            .await?;
        let (pid, start, token) =
            server_fields(output.trim_end()).context("invalid tmux generation")?;
        if token.is_empty() {
            return Ok(GenerationSample::Unstamped { pid, start });
        }
        generation(output.trim_end())
            .map(GenerationSample::Stamped)
            .context("invalid tmux generation token")
    }

    async fn generation(&self) -> Result<ServerGeneration> {
        match self.sample_generation().await? {
            GenerationSample::Stamped(server) => Ok(server),
            GenerationSample::Unstamped { .. } => anyhow::bail!("tmux generation unavailable"),
        }
    }

    async fn establish_generation(&self, expected: ServerGeneration) -> Result<ServerGeneration> {
        match self.sample_generation().await? {
            GenerationSample::Stamped(server) if server == expected => Ok(server),
            GenerationSample::Unstamped { pid, start }
                if (pid, start) == (expected.pid, expected.start) =>
            {
                self.stamp_generation(expected, None).await
            }
            _ => bail!("tmux generation changed"),
        }
    }

    async fn stamp_generation(
        &self,
        expected: ServerGeneration,
        terminal: Option<&GeometryReadback>,
    ) -> Result<ServerGeneration> {
        let ServerGeneration { pid, start, token } = expected;
        // @invariant: The tmux server checks its identity and the empty token before setting the token.
        let mut guards = vec![
            format_equal("after-if-shell", ""),
            format_equal("after-set-option", ""),
            format_equal("command-error", ""),
        ];
        let mut args = vec!["if-shell", "-F"];
        if let Some(read) = terminal {
            args.extend(["-t", &read.target.pane]);
            guards.push(read.guard_with_generation(read.epoch, ""));
            guards.push(format_equal(DIAGNOSTIC_OPTION, ""));
        } else {
            guards.extend([
                format_equal("pid", pid),
                format_equal("start_time", start),
                format_equal(GENERATION_OPTION, ""),
            ]);
        }
        let guard = format_all(guards);
        let set = format!("set-option -s {GENERATION_OPTION} {token}");
        args.extend([guard.as_str(), set.as_str()]);
        self.command(&args).await?;
        ensure!(
            self.generation().await? == expected,
            "tmux generation changed"
        );
        Ok(expected)
    }

    async fn terminal_generation(
        &self,
        runtime: &Runtime,
        catalog: Option<&CatalogLane>,
    ) -> Result<ServerGeneration> {
        let (pid, start) = match self.sample_generation().await? {
            GenerationSample::Stamped(server) => return Ok(server),
            GenerationSample::Unstamped { pid, start } => (pid, start),
        };
        let candidate = ServerGeneration {
            pid,
            start,
            token: Uuid::new_v4(),
        };
        let Some(catalog) = catalog else {
            return self.stamp_generation(candidate, None).await;
        };
        let id = runtime.runtime_id;
        let records = Self::runtime_catalog(id, runtime.session_id, catalog)
            .await
            .map_err(|issue| anyhow::anyhow!("Terminal generation unavailable: {issue:?}"))?;
        ensure!(
            records.managed.is_none() && records.diagnostic.is_none(),
            "Managed terminal generation is unavailable"
        );
        let output = self.geometry_output(&runtime.pane_id).await?;
        let fields = Self::geometry_fields(&output)?;
        ensure!(
            server_fields(&fields[..3].join("|")) == Some((pid, start, ""))
                && fields[7] == runtime.tmux_name
                && fields[10].is_empty()
                && fields[11] == if runtime.exited { "1" } else { "0" },
            "Unstamped terminal target changed"
        );
        Self::check_geometry_layout(&fields)?;
        let (target, state, server) =
            self.geometry_identity(&fields, id, runtime.exited, None, Some(candidate))?;
        let read = self.decode_geometry(&fields, target, state, server)?;
        let actual_pid = match read.state {
            PaneState::Live => Some(read.target.pid),
            PaneState::Exited(pid) => pid,
        };
        ensure!(
            read.target.pane == runtime.pane_id && actual_pid == runtime.pane_pid,
            "Unstamped terminal target changed"
        );
        let scope = format!(
            "{pid}|{start}||{}|{}",
            read.target.session, read.target.window
        );
        self.corroborate_legacy(
            &runtime.pane_id,
            &runtime.tmux_name,
            runtime.pane_pid,
            runtime.exited,
            &scope,
        )
        .await?;
        if !runtime.exited {
            tty_identity(&fs::symlink_metadata(&read.tty)?)?;
            ensure!(
                self.observed_process_start(read.target.pid)? == read.target.process_start,
                "Unstamped terminal process changed"
            );
        }
        self.stamp_generation(candidate, Some(&read)).await
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
        let sample = lines.next().context("Missing tmux generation")?;
        ensure!(
            generation(sample) == Some(expected)
                || server_fields(sample) == Some((expected.pid, expected.start, "")),
            "tmux generation changed"
        );
        Ok(lines
            .filter(|name| parse_name(name).is_some())
            .map(str::to_owned)
            .collect())
    }

    async fn creation_targets(
        &self,
        catalog: &CatalogLane,
        server: ServerGeneration,
    ) -> Result<HashSet<(Uuid, Uuid)>, ProductIssue> {
        // @invariant: Sample marks first so late effects settled during discovery are still counted.
        let marked = catalog.run(move |db, _| db.marked_targets(server)).await?;
        let names = self.names(server).await.map_err(|_| unavailable())?;
        let mut targets = HashSet::new();
        let mut sessions = HashSet::new();
        let mut runtimes = HashSet::new();
        for name in names.into_iter().chain(marked) {
            let ids = parse_name(&name)
                .filter(|(session, runtime)| tmux_name(*session, *runtime) == name)
                .ok_or_else(unavailable)?;
            if targets.insert(ids) && (!sessions.insert(ids.0) || !runtimes.insert(ids.1)) {
                return Err(unavailable());
            }
        }
        Ok(targets)
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
        Self::decode_pane_evidence(name, server, nonce, lines)
    }

    fn decode_pane_evidence<'a>(
        name: &str,
        server: ServerGeneration,
        nonce: &str,
        mut lines: impl Iterator<Item = &'a str>,
    ) -> Result<PaneEvidence> {
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

    async fn sample_diagnostic(
        &self,
        name: &str,
        generation: ServerGeneration,
        nonce: &str,
    ) -> Result<PaneEvidence> {
        let target = format!("={name}:0");
        let format = EVIDENCE_FORMAT.replace(
            "#{@rs_create_nonce}",
            "#{?@rs_create_nonce,invalid,#{@rs_diagnostic_create}}",
        );
        let output = self
            .command(&["list-panes", "-s", "-t", &target, "-F", &format])
            .await?;
        let evidence = Self::decode_pane_evidence(name, generation, nonce, output.lines())?;
        if !evidence.exited {
            self.observed_process_start(evidence.identity.pid)?;
        }
        Ok(evidence)
    }

    async fn observe_diagnostic(&self, mark: &DiagnosticCreate) -> Result<PaneEvidence> {
        let evidence = self
            .sample_diagnostic(&mark.target, mark.generation, &mark.nonce.to_string())
            .await?;
        ensure!(
            mark.pane
                .as_ref()
                .is_none_or(|pane| *pane == evidence.identity),
            "Diagnostic creation target changed"
        );
        Ok(evidence)
    }

    async fn settle_diagnostic(
        catalog: &CatalogLane,
        mark: DiagnosticCreate,
        identity: PaneIdentity,
    ) -> Result<()> {
        if mark.pane.as_ref() == Some(&identity) {
            return Ok(());
        }
        ensure!(mark.pane.is_none(), "Diagnostic creation target changed");
        let settled = catalog
            .run(move |db, _| db.settle_diagnostic(&mark, &identity))
            .await
            .map_err(|issue| anyhow::anyhow!("Diagnostic settlement unavailable: {issue:?}"))?;
        ensure!(settled, "Diagnostic settlement was not acknowledged");
        Ok(())
    }

    fn spawn_command(&self, operation: &Operation) -> Result<(String, String)> {
        let service_state = self
            .socket
            .parent()
            .context("missing service state")?
            .to_owned();
        let launcher = std::env::current_exe()?;
        #[cfg(test)]
        let launcher = self.paste_test.launch_program.clone().unwrap_or(launcher);
        let launcher = workspace_locator(&launcher)
            .map_err(|_| anyhow::anyhow!("unsupported host binary path"))?;
        let cwd = workspace_locator(Path::new(&operation.policy.workspace.canonical))
            .map_err(|_| anyhow::anyhow!("unsupported launch directory"))?;
        let ticket = Uuid::new_v4();
        let capsule = LaunchGuard {
            locator: operation.locator.clone(),
            profile: operation.profile,
            policy: operation.policy.clone(),
            service_state,
            nonce: operation.nonce,
            birth: Some(LaunchBirth {
                tmux: fs::canonicalize(&self.tmux)?,
                server: operation.generation,
                operation: operation.id,
                session: operation.session_id,
                runtime: operation.runtime_id,
                ticket,
            }),
        }
        .argument()?;
        let guard = creation_guard(operation.generation);
        let name = &operation.target;
        let pane_target = format!("={name}:0.0");
        let nonce = operation.nonce;
        let prefix = birth_prefix(
            operation.generation,
            operation.id,
            operation.runtime_id,
            nonce,
        );
        // @security: Double literal hashes to prevent tmux format expansion in the working directory.
        let cwd = tmux_word(&cwd.replace('#', "##"));
        let launcher = tmux_word(launcher);
        let profile = operation.profile.executable_name();
        let branch = format!(
            "new-session -d -E -s {name} -n {profile} -c {cwd} -x 100 -y 30 \
             -e {NONCE_ENV}={nonce} -e {TICKET_ENV}={ticket} -P -F '{RECEIPT_FORMAT}' -- {launcher} {LAUNCH_COMMAND} {capsule} ; \
             set-environment -r -t ={name} {TICKET_ENV} ; \
             set-option -Fpo -t {pane_target} {INITIALIZER_OPTION} '{prefix}{ticket}~#{{session_id}}~#{{window_id}}~#{{pane_id}}~#{{pane_pid}}' ; \
             set-option -Fpo -t {pane_target} {PANE_OPTION} '{nonce}:#{{session_id}}:#{{window_id}}:#{{pane_id}}:#{{pane_pid}}' ; \
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
        let Ok(sample) = self.sample_generation().await else {
            return unknown(unavailable());
        };
        let generation = sample.candidate();
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
        let targets = match self.creation_targets(catalog, generation).await {
            Ok(targets) => targets,
            Err(issue) => return unknown(issue),
        };
        if targets
            .iter()
            .any(|(session, runtime)| *session == operation.session_id || *runtime == runtime_id)
        {
            return unknown(None);
        }
        if targets.len() >= RUNTIME_LIMIT {
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
        if self.establish_generation(generation).await.is_err() {
            return unknown(unavailable());
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
                    Ok(observed) if observed.identity == *identity => {
                        if observed.runtime.exited {
                            PaneLiveness::Exited
                        } else if self
                            .prove_observed_managed(&operation, &observed, false)
                            .await
                            .is_ok()
                        {
                            PaneLiveness::Live
                        } else {
                            PaneLiveness::Unavailable
                        }
                    }
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
        if !observed.runtime.exited {
            let proof = match self
                .prove_observed_managed(operation, &observed, true)
                .await
            {
                Ok(ManagedProvenance::Historical(proof)) if !proof.ready => return unknown(None),
                Ok(proof) => proof,
                Err(_) => return unknown(None),
            };
            if let ManagedProvenance::Birth(birth) = proof
                && !birth.used
            {
                let current = match Self::load_operation(catalog, operation.key).await {
                    Ok(Some(current)) => current,
                    Ok(None) => return unknown(None),
                    Err(issue) => return unknown(issue),
                };
                if current.stage != OperationStage::Marked
                    || current.id != operation.id
                    || current.generation != operation.generation
                    || current.nonce != operation.nonce
                    || current.target != operation.target
                {
                    return unknown(None);
                }
                let _ = self
                    .initialize_managed_writer(operation, &observed.identity, &birth)
                    .await;
                let Ok(after) = self.prove_live_birth(operation, &observed.identity).await else {
                    return unknown(None);
                };
                if after.record != birth.record {
                    return unknown(None);
                }
            }
        }
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
        let deadline = tokio::time::Instant::now() + INVENTORY_BUDGET;
        let mut rows = self.list_runtimes(true, Some(deadline)).await?;
        for (runtime, _) in &mut rows {
            runtime.origin = RuntimeOrigin::Managed;
        }
        if rows.is_empty()
            || deadline.saturating_duration_since(tokio::time::Instant::now())
                < INVENTORY_PROOF_MIN_BUDGET
        {
            return Ok(rows.into_iter().map(|(runtime, _)| runtime).collect());
        }
        let operations = if let Some(catalog) = catalog {
            let ids: Vec<_> = rows.iter().map(|(runtime, _)| runtime.runtime_id).collect();
            tokio::time::timeout_at(deadline, catalog.run(move |db, _| db.by_runtime(&ids)))
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let _ = tokio::time::timeout_at(deadline, async {
            for (runtime, scope) in &mut rows {
                if tokio::time::Instant::now() >= deadline {
                    break;
                }
                let Some(proof) = scope else { continue };
                let fields: Vec<_> = proof.split('|').collect();
                let [_, _, _, session, window] = fields.as_slice() else {
                    continue;
                };
                let server = fields[..3].join("|");
                let managed_generation = generation(&server);
                if !canonical_tmux_id(session, '$')
                    || !canonical_tmux_id(window, '@')
                    || !server_fields(&server).is_some_and(|(_, _, token)| {
                        token.is_empty() || managed_generation.is_some()
                    })
                {
                    continue;
                }
                if let Some(operation) = operations
                    .iter()
                    .find(|op| op.runtime_id == runtime.runtime_id)
                {
                    if let OperationStage::Finished(StoredOutcome::Launched { identity, .. }) =
                        &operation.stage
                        && Some(operation.generation) == managed_generation
                        && operation.session_id == runtime.session_id
                        && operation.target == runtime.tmux_name
                        && identity.session == *session
                        && identity.window == *window
                        && identity.pane == runtime.pane_id
                        && let Ok(observed) = self.observe(operation).await
                        && observed.identity == *identity
                        && observed.runtime.pane_pid == runtime.pane_pid
                        && observed.runtime.exited == runtime.exited
                        && (runtime.exited
                            || self
                                .prove_observed_managed(operation, &observed, false)
                                .await
                                .is_ok())
                    {
                        *runtime = observed.runtime;
                    }
                } else if self
                    .corroborate_legacy(
                        &runtime.pane_id,
                        &runtime.tmux_name,
                        runtime.pane_pid,
                        runtime.exited,
                        proof,
                    )
                    .await
                    .is_ok()
                {
                    runtime.origin = RuntimeOrigin::Legacy;
                }
            }
        })
        .await;
        Ok(rows.into_iter().map(|(runtime, _)| runtime).collect())
    }

    async fn corroborate_legacy(
        &self,
        pane: &str,
        name: &str,
        pid: Option<u32>,
        exited: bool,
        scope: &str,
    ) -> Result<()> {
        let session = format!("={name}");
        let output = self
            .command(&[
                "display-message",
                "-p",
                "-t",
                pane,
                INVENTORY_PANE_FORMAT,
                ";",
                "show-environment",
                "-s",
                "-t",
                &session,
                ";",
                "show-options",
                "-p",
                "-t",
                pane,
                ";",
                "display-message",
                "-p",
                "-t",
                pane,
                INVENTORY_PANE_FORMAT,
            ])
            .await?;
        let mut lines = output.lines();
        let before = lines.next().context("Missing inventory pane proof")?;
        let after = lines
            .next_back()
            .context("Incomplete inventory pane proof")?;
        let fields: Vec<_> = before.split('|').collect();
        let expected_pid = pid.map(|pid| pid.to_string());
        let expected_dead = if exited { "1" } else { "0" };
        ensure!(
            before == after
                && fields.len() == 10
                && fields[..5].join("|") == scope
                && fields[5] == pane
                && fields[6] == expected_pid.as_deref().unwrap_or_default()
                && fields[7] == name
                && fields[8] == expected_dead
                && fields[9].is_empty()
                && !lines.any(|line| {
                    line.starts_with(PANE_OPTION)
                        || line
                            .strip_prefix("unset ")
                            .is_some_and(|name| name.trim_end_matches(';') == NONCE_ENV)
                        || line
                            .strip_prefix(NONCE_ENV)
                            .is_some_and(|rest| rest.starts_with('='))
                }),
            "Legacy terminal provenance is uncorroborated"
        );
        Ok(())
    }

    async fn geometry(&self, runtime_id: Uuid, pane: &str) -> Result<GeometryReadback> {
        self.geometry_in_state(runtime_id, pane, false).await
    }

    async fn geometry_in_state(
        &self,
        runtime_id: Uuid,
        pane: &str,
        exited: bool,
    ) -> Result<GeometryReadback> {
        let output = self.geometry_output(pane).await?;
        let fields = Self::geometry_fields(&output)?;
        ensure!(
            parse_name(fields[7]).is_some_and(|(_, id)| id == runtime_id)
                && fields[11] == if exited { "1" } else { "0" },
            "Terminal target changed or is unsupported"
        );
        Self::check_geometry_layout(&fields)?;
        let original_process = if exited && !fields[23].is_empty() {
            Some(paste_process_stamp(fields[23])?)
        } else {
            None
        };
        let (target, state, server) =
            self.geometry_identity(&fields, runtime_id, exited, original_process, None)?;
        self.decode_geometry(&fields, target, state, server)
    }

    async fn geometry_output(&self, pane: &str) -> Result<String> {
        ensure!(canonical_tmux_id(pane, '%'), "Invalid terminal pane ID");
        let format = format!("{GEOMETRY_FORMAT}|{}", self.sender_guard());
        self.geometry_command(&["display-message", "-p", "-t", pane, &format])
            .await
    }

    fn geometry_fields(output: &str) -> Result<[&str; 26]> {
        let mut lines = output.lines();
        let fields: Vec<_> = lines
            .next()
            .context("Missing terminal target")?
            .split('|')
            .collect();
        ensure!(lines.next().is_none(), "Invalid terminal metadata");
        fields
            .try_into()
            .map_err(|_| anyhow::anyhow!("Invalid terminal metadata"))
    }

    fn check_geometry_layout(fields: &[&str; 26]) -> Result<()> {
        ensure!(
            fields[8..10] == ["0", "0"] && fields[12..15] == ["1", "1", "manual"],
            "Terminal target changed or is unsupported"
        );
        Ok(())
    }

    fn geometry_identity(
        &self,
        fields: &[&str; 26],
        runtime_id: Uuid,
        exited: bool,
        original_process: Option<(&str, u64)>,
        unstamped: Option<ServerGeneration>,
    ) -> Result<(TerminalTarget, PaneState, ServerGeneration)> {
        let server = unstamped
            .or_else(|| generation(&fields[..3].join("|")))
            .context("Invalid terminal generation")?;
        let current_pid = match fields[6] {
            "" if exited => None,
            pid => Some(pid.parse::<u32>()?),
        };
        let stamp = fields[10].strip_prefix('~').unwrap_or(fields[10]);
        let identity = if stamp.is_empty() {
            let pid = match (current_pid, original_process) {
                (None, Some((original_pid, _))) => original_pid,
                _ => fields[6],
            };
            pane_identity(&[fields[3], fields[4], fields[5], pid])
                .context("Invalid terminal pane")?
        } else {
            let (nonce, ids) = stamp
                .split_once(':')
                .context("Invalid terminal launch stamp")?;
            parse_uuid(nonce)?;
            let parsed: Vec<_> = ids.split(':').collect();
            let identity = pane_identity(&parsed).context("Invalid terminal launch stamp")?;
            ensure!(
                fields[3..6] == parsed[..3] && current_pid.is_none_or(|pid| pid == identity.pid),
                "Terminal launch stamp changed"
            );
            identity
        };
        let process_start = if exited {
            original_process.map_or(0, |(_, started)| started)
        } else {
            self.observed_process_start(identity.pid)?
        };
        let target = TerminalTarget {
            runtime_id,
            generation: server.token,
            session: identity.session,
            window: identity.window,
            pane: identity.pane,
            pid: identity.pid,
            process_start,
        };
        let state = if exited {
            PaneState::Exited(current_pid)
        } else {
            PaneState::Live
        };
        Ok((target, state, server))
    }

    fn observed_process_start(&self, pid: u32) -> Result<u64> {
        let start = process_start(pid)?;
        #[cfg(test)]
        let start = self.paste_test.replacement_start.unwrap_or(start);
        Ok(start)
    }

    fn decode_geometry(
        &self,
        fields: &[&str; 26],
        target: TerminalTarget,
        state: PaneState,
        server: ServerGeneration,
    ) -> Result<GeometryReadback> {
        let tty = fields[15];
        ensure!(valid_pane_tty(tty), "Unsupported terminal device");
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
        #[cfg(debug_assertions)]
        ensure!(
            self.resize_fixture_target
                .as_ref()
                .is_none_or(|expected| *expected == target),
            "Resize fixture target changed"
        );
        let epoch = parse_epoch(fields[20])?;
        let committed = parse_committed(fields[21], size)?;
        ensure!(
            committed.intent.is_none() || epoch.is_some(),
            "Resize intent has no engine epoch"
        );
        let paste_launch = match fields[22].split_once('~') {
            Some((launch, birth))
                if !birth
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"/$@%~-".contains(&byte)) =>
            {
                format!("{launch}~invalid")
            }
            _ => fields[22].to_owned(),
        };
        Ok(GeometryReadback {
            target,
            state,
            server,
            name: fields[7].to_owned(),
            nonce: if fields[10].starts_with('~') {
                String::new()
            } else {
                fields[10].to_owned()
            },
            diagnostic: fields[10].strip_prefix('~').map(str::to_owned),
            tty: tty.into(),
            epoch,
            committed,
            paste_launch,
            paste_marker: fields[23].into(),
            input_enabled: fields[24] == "0",
            sender_supported: matches!(fields[24], "0" | "1")
                && fields[25] == "1"
                && self.private_sender_socket(),
        })
    }

    async fn geometry_for_runtime(
        &self,
        runtime: &Runtime,
        server: ServerGeneration,
    ) -> Result<GeometryReadback> {
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

    async fn retained_geometry(
        &self,
        runtime: &Runtime,
        server: ServerGeneration,
    ) -> Result<GeometryReadback> {
        let read = self
            .geometry_in_state(runtime.runtime_id, &runtime.pane_id, true)
            .await?;
        let target = &read.target;
        ensure!(
            read.server == server
                && read.name == runtime.tmux_name
                && target.pane == runtime.pane_id
                && read.state == PaneState::Exited(runtime.pane_pid),
            "Retained Stop target changed"
        );
        Ok(read)
    }

    async fn runtime_catalog(
        runtime: Uuid,
        session: Uuid,
        catalog: &CatalogLane,
    ) -> Result<RuntimeCatalog, ProductIssue> {
        catalog
            .run(move |db, _| {
                let managed = db.operation_for_target(runtime, session)?;
                let diagnostic = db.diagnostic(runtime, session)?;
                if diagnostic.as_ref().is_some_and(|mark| {
                    (mark.runtime_id, mark.session_id) != (runtime, session) || managed.is_some()
                }) {
                    return Err(ProductIssue::CreateConflict);
                }
                Ok(RuntimeCatalog {
                    managed,
                    diagnostic,
                })
            })
            .await
    }

    async fn terminal_catalog(
        read: &GeometryIdentity<'_>,
        catalog: Option<&CatalogLane>,
    ) -> Result<RuntimeCatalog, ProductIssue> {
        let runtime_id = read.target.runtime_id;
        let (session_id, _) = parse_name(read.name)
            .filter(|(session, runtime)| {
                *runtime == runtime_id && tmux_name(*session, *runtime) == read.name
            })
            .ok_or(ProductIssue::InvalidRequest)?;
        match catalog {
            Some(lane) => Self::runtime_catalog(runtime_id, session_id, lane).await,
            None => Err(ProductIssue::CatalogUnconfigured),
        }
    }

    async fn corroborate_terminal(
        &self,
        read: &GeometryIdentity<'_>,
        nonce: &str,
    ) -> Result<PaneIdentity> {
        ensure!(
            read.nonce
                .split_once(':')
                .is_some_and(|(stamp, _)| stamp == nonce),
            "Managed terminal launch stamp changed"
        );
        let evidence = self.observe_pane(read.name, read.server, nonce).await?;
        let identity = evidence.identity;
        let target = read.target;
        let matches_state = match read.state {
            PaneState::Live => {
                !evidence.exited
                    && evidence.pane_pid == Some(target.pid)
                    && self.observed_process_start(target.pid)? == target.process_start
            }
            PaneState::Exited(pid) => evidence.exited && evidence.pane_pid == pid,
        };
        ensure!(
            matches_state
                && identity.session == target.session
                && identity.window == target.window
                && identity.pane == target.pane
                && identity.pid == target.pid,
            "Managed terminal identity changed"
        );
        Ok(identity)
    }

    async fn verify_historical(&self, read: &GeometryReadback) -> Result<HistoricalProof> {
        let launch = read
            .historical_launch()
            .context("Missing historical launch")?;
        let (session, runtime) = parse_name(&read.name).context("Invalid historical target")?;
        let (nonce, _) = read
            .nonce
            .split_once(':')
            .context("Missing historical nonce")?;
        let target = &read.target;
        let ready = read.paste_ready();
        ensure!(
            read.state == PaneState::Live
                && !session.is_nil()
                && !runtime.is_nil()
                && runtime == target.runtime_id
                && read.name == tmux_name(session, runtime)
                && parse_uuid(nonce).is_ok()
                && read.diagnostic.is_none()
                && (ready || PasteBuffer::from_marker(read).is_some()),
            "Historical writer marker is unconfirmed"
        );
        // @invariant: Historical markers bind start; verify the current TTY device around identity reads.
        let device = tty_identity(&fs::symlink_metadata(&read.tty)?)?;
        let first = self.corroborate_terminal(&read.identity(), nonce).await?;
        let birth = self
            .sample_live_birth(
                BirthTarget {
                    name: &read.name,
                    server: read.server,
                    nonce: parse_uuid(nonce)?,
                },
                &first,
            )
            .await?;
        ensure!(
            birth.tty == read.tty && birth.initializer.is_empty() && birth.record.is_empty(),
            "Historical terminal has initializer or birth metadata"
        );
        let pane = &target.pane;
        ensure!(
            self.geometry_command(&[
                "show-options",
                "-pq",
                "-t",
                pane,
                INITIALIZER_OPTION,
                ";",
                "show-options",
                "-pq",
                "-t",
                pane,
                BIRTH_OPTION,
            ])
            .await?
            .is_empty(),
            "Historical terminal has initializer or birth metadata"
        );
        let after = self.geometry(runtime, pane).await?;
        ensure!(
            after.same_pane(read)
                && after.epoch == read.epoch
                && after.paste_marker == read.paste_marker
                && self.corroborate_terminal(&after.identity(), nonce).await? == first
                && tty_identity(&fs::symlink_metadata(&after.tty)?)? == device,
            "Historical process changed during proof"
        );
        Ok(HistoricalProof {
            origin: HistoricalOrigin {
                server: read.server,
                name: read.name.clone(),
                nonce: read.nonce.clone(),
                launch,
                tty: read.tty.clone(),
                device,
            },
            marker: read.paste_marker.clone(),
            ready,
        })
    }

    async fn prove_observed_managed(
        &self,
        operation: &Operation,
        observed: &Observation,
        wait_for_record: bool,
    ) -> Result<ManagedProvenance> {
        match self
            .verify_live_birth(operation, &observed.identity, wait_for_record)
            .await
        {
            Ok(birth) => Ok(ManagedProvenance::Birth(birth)),
            Err(birth_error) => {
                let read = self
                    .geometry_for_runtime(&observed.runtime, operation.generation)
                    .await?;
                self.verify_historical_operation(&read.identity(), operation, &observed.identity)
                    .await
                    .map(ManagedProvenance::Historical)
                    .with_context(|| birth_error.to_string())
            }
        }
    }

    async fn verify_historical_operation(
        &self,
        read: &GeometryIdentity<'_>,
        operation: &Operation,
        identity: &PaneIdentity,
    ) -> Result<HistoricalProof> {
        ensure!(
            operation.target == tmux_name(operation.session_id, operation.runtime_id)
                && read.name == operation.target
                && read.target.runtime_id == operation.runtime_id
                && read.server == operation.generation
                && read.target.session == identity.session
                && read.target.window == identity.window
                && read.target.pane == identity.pane
                && read.target.pid == identity.pid
                && read
                    .nonce
                    .split_once(':')
                    .is_some_and(|(nonce, _)| parse_uuid(nonce).ok() == Some(operation.nonce)),
            "Historical catalog identity changed"
        );
        let sampled;
        let geometry = match read.sample {
            Some(geometry) => geometry,
            None => {
                sampled = self
                    .geometry(read.target.runtime_id, &read.target.pane)
                    .await?;
                ensure!(
                    &sampled.target == read.target
                        && sampled.server == read.server
                        && sampled.name == read.name
                        && sampled.nonce == read.nonce
                        && sampled.tty == read.tty,
                    "Historical terminal sample changed"
                );
                &sampled
            }
        };
        self.verify_historical(geometry).await
    }

    fn remember_historical_view(&self, terminal: &TerminalView, proof: Option<&ManagedProvenance>) {
        let mut views = self.historical_views.lock().expect("historical view lock");
        views.remove(&terminal.view_id);
        if let Some(ManagedProvenance::Historical(proof)) = proof {
            let now = self.now();
            views.retain(|_, view| view.expires > now);
            if views.len() == HISTORICAL_VIEW_LIMIT
                && let Some(id) = views
                    .iter()
                    .min_by_key(|(_, view)| view.expires)
                    .map(|(id, _)| *id)
            {
                views.remove(&id);
            }
            views.insert(
                terminal.view_id,
                IssuedHistoricalView {
                    terminal: terminal.clone(),
                    origin: proof.origin.clone(),
                    expires: now + HISTORICAL_VIEW_TTL,
                },
            );
        }
    }

    fn historical_view(&self, terminal: &TerminalView) -> Option<HistoricalOrigin> {
        self.historical_views
            .lock()
            .expect("historical view lock")
            .get(&terminal.view_id)
            .filter(|view| view.terminal == *terminal && view.expires > self.now())
            .map(|view| view.origin.clone())
    }

    fn refresh_historical_view(&self, terminal: &TerminalView, proof: &HistoricalProof) {
        let mut views = self.historical_views.lock().expect("historical view lock");
        if let Some(view) = views.get_mut(&terminal.view_id)
            && view.terminal == *terminal
            && view.origin == proof.origin
        {
            view.expires = self.now() + HISTORICAL_VIEW_TTL;
        }
    }

    async fn verify_terminal_operation(
        &self,
        read: &GeometryIdentity<'_>,
        operation: Option<&Operation>,
    ) -> Result<Option<ManagedProvenance>> {
        ensure!(
            read.diagnostic.is_none(),
            "Diagnostic terminal is uncorroborated"
        );
        let Some(operation) = operation else {
            ensure!(read.nonce.is_empty(), "Managed terminal is uncorroborated");
            return self.prove_legacy_terminal(read).await.map(|()| None);
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
        let birth = self
            .verify_terminal_birth(read, operation, identity)
            .await
            .context(UnprovenTerminalBirth)?;
        ensure!(
            birth.as_ref().is_none_or(|proof| proof.tty() == read.tty),
            "Managed terminal TTY changed"
        );
        Ok(birth)
    }

    async fn verify_terminal_birth(
        &self,
        read: &GeometryIdentity<'_>,
        operation: &Operation,
        identity: &PaneIdentity,
    ) -> Result<Option<ManagedProvenance>> {
        if read.state != PaneState::Live {
            return Ok(None);
        }
        match self.verify_live_birth(operation, identity, false).await {
            Ok(birth) => {
                ensure!(
                    birth.start == read.target.process_start,
                    "Managed terminal birth changed"
                );
                Ok(Some(ManagedProvenance::Birth(birth)))
            }
            Err(birth_error) => self
                .verify_historical_operation(read, operation, identity)
                .await
                .map(|proof| Some(ManagedProvenance::Historical(proof)))
                .with_context(|| birth_error.to_string()),
        }
    }

    async fn verify_diagnostic_terminal(
        &self,
        read: &GeometryIdentity<'_>,
        mark: &DiagnosticCreate,
    ) -> Result<PaneIdentity> {
        ensure!(
            read.nonce.is_empty() && read.name == mark.target && read.server == mark.generation,
            "Diagnostic terminal identity changed"
        );
        let nonce = mark.nonce.to_string();
        let evidence = self.observe_diagnostic(mark).await?;
        self.match_diagnostic_terminal(read, &nonce, evidence)
    }

    fn match_diagnostic_terminal(
        &self,
        read: &GeometryIdentity<'_>,
        nonce: &str,
        evidence: PaneEvidence,
    ) -> Result<PaneIdentity> {
        let pane = evidence.identity;
        let target = read.target;
        let stamp = format!(
            "{nonce}:{}:{}:{}:{}",
            pane.session, pane.window, pane.pane, pane.pid
        );
        let matches_state = match read.state {
            PaneState::Live => {
                !evidence.exited
                    && evidence.pane_pid == Some(target.pid)
                    && self.observed_process_start(target.pid)? == target.process_start
            }
            PaneState::Exited(current) => evidence.exited && evidence.pane_pid == current,
        };
        ensure!(
            matches_state
                && read.diagnostic == Some(stamp.as_str())
                && (
                    pane.session.as_str(),
                    pane.window.as_str(),
                    pane.pane.as_str(),
                    pane.pid
                ) == (
                    target.session.as_str(),
                    target.window.as_str(),
                    target.pane.as_str(),
                    target.pid
                ),
            "Diagnostic terminal stamp or tuple changed"
        );
        Ok(pane)
    }

    async fn observe_terminal(
        &self,
        read: &GeometryIdentity<'_>,
        catalog: Option<&CatalogLane>,
    ) -> Result<Option<ManagedProvenance>> {
        if catalog.is_none() && read.nonce.is_empty() && read.diagnostic.is_none() {
            return Ok(None);
        }
        self.observe_terminal_records(read, Self::terminal_catalog(read, catalog).await, false)
            .await
    }

    async fn prove_independent_live_birth(&self, read: &GeometryIdentity<'_>) -> Result<bool> {
        ensure!(
            read.state == PaneState::Live,
            "Managed process birth is unavailable"
        );
        let (nonce, _) = read
            .nonce
            .split_once(':')
            .context("Invalid managed launch stamp")?;
        let identity = self.corroborate_terminal(read, nonce).await?;
        let target = BirthTarget {
            name: read.name,
            server: read.server,
            nonce: parse_uuid(nonce)?,
        };
        let sample = self.sample_live_birth(target, &identity).await?;
        let operation = parse_uuid(
            sample
                .initializer
                .split('~')
                .nth(4)
                .context("Missing birth operation")?,
        )?;
        let birth = self
            .verify_birth(target, operation, read.target.runtime_id, &identity, false)
            .await?;
        ensure!(
            birth.start == read.target.process_start,
            "Managed terminal birth changed"
        );
        Ok(birth.tty == read.tty)
    }

    async fn observe_terminal_records(
        &self,
        read: &GeometryIdentity<'_>,
        records: Result<RuntimeCatalog, ProductIssue>,
        bound: bool,
    ) -> Result<Option<ManagedProvenance>> {
        match records {
            Ok(RuntimeCatalog {
                diagnostic: Some(mark),
                ..
            }) => {
                self.verify_diagnostic_terminal(read, &mark).await?;
                Ok(None)
            }
            Ok(records) => {
                self.verify_terminal_operation(read, records.managed.as_ref())
                    .await
            }
            Err(issue)
                if bound
                    && (issue.storage_fault() || issue == ProductIssue::WorkspaceCheckTimedOut) =>
            {
                if let Some(stamp) = read.diagnostic {
                    let (nonce, _) = stamp
                        .split_once(':')
                        .context("Invalid diagnostic terminal stamp")?;
                    let evidence = self
                        .sample_diagnostic(read.name, read.server, nonce)
                        .await?;
                    self.match_diagnostic_terminal(read, nonce, evidence)?;
                } else if read.nonce.is_empty() {
                    self.prove_legacy_terminal(read).await?;
                } else {
                    ensure!(
                        self.prove_independent_live_birth(read).await?,
                        "Managed terminal TTY changed"
                    );
                }
                Ok(None)
            }
            Err(issue) => bail!("Managed terminal catalog unavailable: {issue:?}"),
        }
    }

    async fn prove_legacy_terminal(&self, read: &GeometryIdentity<'_>) -> Result<()> {
        let server = read.server;
        let target = read.target;
        let scope = format!(
            "{}|{}|{}|{}|{}",
            server.pid, server.start, server.token, target.session, target.window
        );
        let (pid, exited) = match read.state {
            PaneState::Live => (Some(target.pid), false),
            PaneState::Exited(pid) => (pid, true),
        };
        self.corroborate_legacy(&target.pane, read.name, pid, exited, &scope)
            .await?;
        if !exited {
            ensure!(
                self.observed_process_start(target.pid)? == target.process_start,
                "Terminal process identity is unavailable"
            );
        }
        Ok(())
    }

    async fn observe_status_identity(
        &self,
        read: &GeometryIdentity<'_>,
        catalog: Option<&CatalogLane>,
        terminal: Option<&TerminalView>,
    ) -> Result<bool, TerminalStatusFailure> {
        if catalog.is_none() && read.nonce.is_empty() && read.diagnostic.is_none() {
            return Ok(true);
        }
        let records = Self::terminal_catalog(read, catalog).await;
        if let Ok(RuntimeCatalog {
            managed: Some(operation),
            diagnostic: None,
        }) = &records
            && let OperationStage::Finished(StoredOutcome::Launched { identity, .. }) =
                &operation.stage
            && read.name == operation.target
        {
            let current = self
                .corroborate_terminal(read, &operation.nonce.to_string())
                .await?;
            if read.server != operation.generation || current != *identity {
                let generation = operation.generation;
                if generation.pid == 0
                    || generation.start == 0
                    || generation.token.is_nil()
                    || !canonical_tmux_id(&identity.session, '$')
                    || !canonical_tmux_id(&identity.window, '@')
                    || !canonical_tmux_id(&identity.pane, '%')
                {
                    return Err(anyhow::anyhow!("Invalid catalog terminal identity").into());
                }
                return Err(TerminalStatusFailure::Changed);
            }
            let proof = self
                .verify_terminal_birth(read, operation, identity)
                .await?;
            if let (Some(terminal), Some(ManagedProvenance::Historical(proof))) = (terminal, &proof)
            {
                self.refresh_historical_view(terminal, proof);
            }
            return Ok(proof.is_none_or(|proof| proof.tty() == read.tty));
        }
        if let Some(terminal) = terminal
            && let Some(sample) = read.sample
            && matches!(records, Err(issue) if issue.storage_fault() || issue == ProductIssue::WorkspaceCheckTimedOut)
            && let Some(origin) = self.historical_view(terminal)
        {
            if self.verify_historical(sample).await?.origin != origin {
                return Err(TerminalStatusFailure::Changed);
            }
            return Ok(true);
        }
        if terminal.is_none()
            && read.state == PaneState::Live
            && !read.nonce.is_empty()
            && matches!(records, Err(issue) if issue.storage_fault() || issue == ProductIssue::WorkspaceCheckTimedOut)
        {
            if !self.prove_independent_live_birth(read).await? {
                return Err(anyhow::anyhow!("Managed terminal TTY changed").into());
            }
            return Ok(true);
        }
        self.observe_terminal_records(read, records, terminal.is_some())
            .await?;
        Ok(true)
    }

    async fn authorize_terminal_writer(
        &self,
        read: &GeometryReadback,
        catalog: Option<&CatalogLane>,
    ) -> Result<()> {
        self.authorize_terminal_mutation(read, catalog, false)
            .await
            .map(drop)
    }

    async fn authorize_terminal_mutation(
        &self,
        read: &GeometryReadback,
        catalog: Option<&CatalogLane>,
        settle_diagnostic: bool,
    ) -> Result<Option<ManagedProvenance>> {
        let identity = &read.identity();
        let Some(catalog) = catalog else {
            ensure!(
                identity.nonce.is_empty() && identity.diagnostic.is_none(),
                "Managed terminal catalog unavailable: CatalogUnconfigured"
            );
            return Ok(None);
        };
        let records = Self::terminal_catalog(identity, Some(catalog))
            .await
            .map_err(|issue| anyhow::anyhow!("Managed terminal catalog unavailable: {issue:?}"))?;
        if let Some(mark) = records.diagnostic {
            let pane = self.verify_diagnostic_terminal(identity, &mark).await?;
            if mark.pane.is_none() {
                ensure!(settle_diagnostic, "Diagnostic launch is unresolved");
                Self::settle_diagnostic(catalog, mark, pane).await?;
            }
            return Ok(None);
        }
        if read.birth_bound()
            && let Some(birth_operation) = read
                .birth()
                .and_then(|birth| birth.split('~').nth(5))
                .and_then(|value| parse_uuid(value).ok())
        {
            ensure!(
                Some(birth_operation) == records.managed.as_ref().map(|operation| operation.id),
                "Managed terminal birth operation changed"
            );
        }
        let proof = self
            .verify_terminal_operation(identity, records.managed.as_ref())
            .await?;
        ensure!(
            !matches!(proof, Some(ManagedProvenance::Birth(_)))
                || !read.paste_ready()
                || read.birth_bound(),
            "Managed writer launch does not match its birth"
        );
        Ok(proof)
    }

    async fn scoped_geometry(&self, terminal: &TerminalView) -> Result<GeometryReadback> {
        ensure!(
            terminal.agent_epoch == self.agent_epoch
                && !terminal.view_id.is_nil()
                && canonical_tmux_id(&terminal.target.session, '$')
                && canonical_tmux_id(&terminal.target.window, '@')
                && canonical_tmux_id(&terminal.target.pane, '%'),
            "Terminal view changed"
        );
        let read = self
            .geometry(terminal.target.runtime_id, &terminal.target.pane)
            .await?;
        ensure!(read.target == terminal.target, "Terminal target changed");
        Ok(read)
    }

    async fn status_geometry(
        &self,
        expected: &TerminalTarget,
        previous_server: Option<ServerGeneration>,
        catalog: Option<&CatalogLane>,
    ) -> Result<GeometryReadback, TerminalStatusFailure> {
        let output = self.geometry_output(&expected.pane).await?;
        let fields = Self::geometry_fields(&output)?;
        let (_, runtime_id) = parse_name(fields[7])
            .filter(|(session, runtime)| !session.is_nil() && !runtime.is_nil())
            .context("Invalid terminal runtime name")?;
        let exited = match fields[11] {
            "0" => false,
            "1" => true,
            _ => return Err(anyhow::anyhow!("Invalid terminal liveness").into()),
        };
        let (target, state, server) =
            self.geometry_identity(&fields, runtime_id, exited, None, None)?;
        let same_target = target.runtime_id == expected.runtime_id
            && target.generation == expected.generation
            && previous_server.is_none_or(|previous| previous == server)
            && target.session == expected.session
            && target.window == expected.window
            && target.pane == expected.pane
            && target.pid == expected.pid
            && (exited || target.process_start == expected.process_start);
        if !same_target {
            let identity = GeometryIdentity {
                target: &target,
                state,
                server,
                name: fields[7],
                nonce: if fields[10].starts_with('~') {
                    ""
                } else {
                    fields[10]
                },
                diagnostic: fields[10].strip_prefix('~'),
                tty: Path::new(fields[15]),
                sample: None,
            };
            if target.runtime_id == expected.runtime_id {
                self.observe_status_identity(&identity, catalog, None)
                    .await?;
            } else {
                self.observe_terminal(&identity, catalog).await?;
            }
            if identity.nonce.is_empty() && identity.diagnostic.is_none() && catalog.is_none() {
                self.prove_legacy_terminal(&identity).await?;
            }
            return Err(TerminalStatusFailure::Changed);
        }
        if exited {
            return Err(anyhow::anyhow!("Terminal pane has exited").into());
        }
        Self::check_geometry_layout(&fields)?;
        Ok(self.decode_geometry(&fields, target, state, server)?)
    }

    fn measure_provider(
        &self,
        before: &GeometryReadback,
    ) -> Result<(ProviderSample, Option<TtyIdentity>)> {
        #[cfg(test)]
        let simulated_sample = self.paste_test.provider_sample;
        #[cfg(not(test))]
        let simulated_sample = None;
        match simulated_sample {
            Some(sample) => Ok((sample, None)),
            None => {
                let (sample, device) = sample_tty(&before.tty)?;
                Ok((sample, Some(device)))
            }
        }
    }

    fn verify_provider_sample(
        before: &GeometryReadback,
        after: &GeometryReadback,
        sample: ProviderSample,
        device: Option<TtyIdentity>,
    ) -> Result<ProviderSample> {
        ensure!(before == after, "Provider sample lost its target identity");
        if let Some(device) = device {
            ensure!(
                tty_identity(&fs::symlink_metadata(&after.tty)?)? == device,
                "Provider sample lost its target identity"
            );
        }
        Ok(sample)
    }

    async fn sample_provider(&self, before: &GeometryReadback) -> Result<ProviderSample> {
        let (sample, device) = self.measure_provider(before)?;
        let after = self
            .geometry(before.target.runtime_id, &before.target.pane)
            .await?;
        Self::verify_provider_sample(before, &after, sample, device)
    }

    async fn observe_terminal_status(
        &self,
        terminal: TerminalView,
        catalog: Option<&CatalogLane>,
    ) -> Result<TerminalStatus, TerminalStatusFailure> {
        if terminal.agent_epoch != self.agent_epoch {
            return Err(TerminalStatusFailure::Changed);
        }
        if terminal.view_id.is_nil() {
            return Err(anyhow::anyhow!("Invalid terminal view").into());
        }
        let before = self
            .status_geometry(&terminal.target, None, catalog)
            .await?;
        let tty_matches = self
            .observe_status_identity(&before.identity(), catalog, Some(&terminal))
            .await?;
        let measurement = tty_matches.then(|| self.measure_provider(&before));
        let after = self
            .status_geometry(&terminal.target, Some(before.server), catalog)
            .await;
        let provider = match after {
            Ok(after) => {
                let tty_matches = self
                    .observe_status_identity(&after.identity(), catalog, Some(&terminal))
                    .await?;
                measurement
                    .filter(|_| tty_matches)
                    .and_then(Result::ok)
                    .and_then(|(sample, device)| {
                        Self::verify_provider_sample(&before, &after, sample, device).ok()
                    })
            }
            Err(TerminalStatusFailure::Changed) => return Err(TerminalStatusFailure::Changed),
            Err(error @ TerminalStatusFailure::Unavailable(_))
                if before.historical_launch().is_some() && !before.nonce.is_empty() =>
            {
                return Err(error);
            }
            Err(TerminalStatusFailure::Unavailable(_)) => None,
        };
        Ok(before.status(terminal, provider))
    }

    async fn sample_live_birth(
        &self,
        target: BirthTarget<'_>,
        identity: &PaneIdentity,
    ) -> Result<BirthSample> {
        let output = self
            .geometry_command(&["display-message", "-p", "-t", &identity.pane, BIRTH_FORMAT])
            .await?;
        let row = output
            .strip_suffix('\n')
            .context("Missing launch birth sample")?;
        let fields: Vec<_> = row.split('|').collect();
        let stamp = format!(
            "{}:{}:{}:{}:{}",
            target.nonce, identity.session, identity.window, identity.pane, identity.pid
        );
        ensure!(
            fields.len() == 15
                && !row.contains('\n')
                && generation(&fields[..3].join("|")) == Some(target.server)
                && pane_identity(&fields[3..7]).as_ref() == Some(identity)
                && fields[7] == target.name
                && fields[8..10] == ["0", "0"]
                && fields[10] == stamp
                && fields[11] == "0"
                && valid_pane_tty(fields[12]),
            "Managed launch target changed"
        );
        Ok(BirthSample {
            tty: fields[12].into(),
            initializer: fields[13].into(),
            record: fields[14].into(),
        })
    }

    async fn prove_live_birth(
        &self,
        operation: &Operation,
        identity: &PaneIdentity,
    ) -> Result<OriginalBirth> {
        self.verify_live_birth(operation, identity, true).await
    }

    async fn verify_live_birth(
        &self,
        operation: &Operation,
        identity: &PaneIdentity,
        wait_for_record: bool,
    ) -> Result<OriginalBirth> {
        ensure!(
            operation.target == tmux_name(operation.session_id, operation.runtime_id),
            "Invalid managed launch target"
        );
        let target = BirthTarget {
            name: &operation.target,
            server: operation.generation,
            nonce: operation.nonce,
        };
        self.verify_birth(
            target,
            operation.id,
            operation.runtime_id,
            identity,
            wait_for_record,
        )
        .await
    }

    async fn verify_birth(
        &self,
        target: BirthTarget<'_>,
        operation: Uuid,
        runtime: Uuid,
        identity: &PaneIdentity,
        wait_for_record: bool,
    ) -> Result<OriginalBirth> {
        let prefix = birth_prefix(target.server, operation, runtime, target.nonce);
        let (sample, ticket) = tokio::time::timeout(BIRTH_READ_BUDGET, async {
            loop {
                let sample = self.sample_live_birth(target, identity).await?;
                let tail = sample
                    .initializer
                    .strip_prefix(&prefix)
                    .context("Missing launch initializer")?;
                let (ticket, _) = tail.split_once('~').context("Invalid launch initializer")?;
                let ticket = parse_uuid(ticket)?;
                ensure!(
                    ticket.get_version_num() == 4
                        && sample.initializer == initializer(&prefix, ticket, identity),
                    "Launch initializer changed"
                );
                if !sample.record.is_empty() {
                    break Ok::<_, anyhow::Error>((sample, ticket));
                }
                ensure!(wait_for_record, "Missing launch birth record");
                tokio::time::sleep(BIRTH_RETRY).await;
            }
        })
        .await
        .context("Launch birth evidence unavailable")??;
        let start = self.observed_process_start(identity.pid)?;
        let device = tty_identity(&fs::symlink_metadata(&sample.tty)?)?;
        let record = birth_text(&sample.initializer, start, &sample.tty, device);
        let (witness, used) = sample
            .record
            .strip_prefix("used~")
            .map_or((sample.record.as_str(), false), |record| (record, true));
        ensure!(witness == record, "Managed process birth changed");
        ensure!(
            self.sample_live_birth(target, identity).await? == sample
                && self.observed_process_start(identity.pid)? == start
                && tty_identity(&fs::symlink_metadata(&sample.tty)?)? == device,
            "Managed process changed during birth proof"
        );
        Ok(OriginalBirth {
            initializer: sample.initializer,
            record,
            ticket,
            start,
            tty: sample.tty,
            device,
            used,
        })
    }

    async fn initialize_managed_writer(
        &self,
        operation: &Operation,
        identity: &PaneIdentity,
        birth: &OriginalBirth,
    ) -> Result<()> {
        let pane = &identity.pane;
        let read = self.geometry(operation.runtime_id, pane).await?;
        let stamp = format!(
            "{}:{}:{}:{pane}:{}",
            operation.nonce, identity.session, identity.window, identity.pid
        );
        ensure!(
            read.sender_supported
                && read.input_enabled
                && read.server == operation.generation
                && read.name == operation.target
                && read.target.session == identity.session
                && read.target.window == identity.window
                && read.target.pane == *pane
                && read.target.pid == identity.pid
                && read.target.process_start == birth.start
                && read.tty == birth.tty
                && read.nonce == stamp
                && read.epoch.is_none()
                && read.paste_launch.is_empty()
                && read.paste_marker.is_empty(),
            "Managed writer initialization is unavailable"
        );
        let (_, device) = sample_tty(&read.tty)?;
        ensure!(device == birth.device, "Managed terminal device changed");
        self.prove_paste_buffer_io(
            &read,
            ProbeSource::Managed {
                operation: operation.id,
                nonce: operation.nonce,
                ticket: birth.ticket,
            },
        )
        .await?;
        let fresh = self.geometry(operation.runtime_id, pane).await?;
        ensure!(
            fresh == read && tty_identity(&fs::symlink_metadata(&fresh.tty)?)? == device,
            "Managed process changed before writer initialization"
        );

        let guard = format_all([
            self.paste_guard(&fresh),
            format_equal("pane_input_off", 0),
            format_equal(INITIALIZER_OPTION, &birth.initializer),
            format_equal(BIRTH_OPTION, &birth.record),
            format_equal("after-if-shell", ""),
        ]);
        let used = format!("used~{}", birth.record);
        let launch = birth_launch_id(birth.ticket);
        let mut stamped = fresh;
        stamped.paste_launch = format!("{launch}~{used}");
        let ready = stamped.ready_marker(None);
        let branch = format!(
            "set-option -p -t {pane} {BIRTH_OPTION} '{used}' ; \
             set-option -po -t {pane} {PASTE_LAUNCH_OPTION} {launch} ; \
             set-option -po -t {pane} {PASTE_OPTION} {ready}"
        );
        self.geometry_command(&["if-shell", "-F", "-t", pane, &guard, &branch])
            .await?;
        let after = self.geometry(operation.runtime_id, pane).await?;
        stamped.paste_marker = ready;
        ensure!(
            after == stamped && after.paste_ready(),
            "Managed writer initialization is unconfirmed"
        );
        Ok(())
    }

    async fn stamp_paste(
        &self,
        runtime: &Runtime,
        server: ServerGeneration,
        identity: &PaneIdentity,
        diagnostic: Option<&str>,
        catalog: Option<&CatalogLane>,
    ) -> Result<()> {
        let read = self.geometry(runtime.runtime_id, &identity.pane).await?;
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
                && read.nonce.is_empty()
                && read.diagnostic.as_deref() == diagnostic
                && read.epoch.is_none()
                && read.paste_launch.is_empty()
                && read.paste_marker.is_empty(),
            "Paste launch provenance is unavailable"
        );
        self.authorize_terminal_writer(&read, catalog).await?;
        let source = match diagnostic {
            Some(stamp) => ProbeSource::Diagnostic(parse_uuid(
                stamp
                    .split_once(':')
                    .context("Missing diagnostic probe stamp")?
                    .0,
            )?),
            None => ProbeSource::CatalogFree,
        };
        let device = self.prove_paste_buffer_io(&read, source).await?;
        let fresh = self.geometry(runtime.runtime_id, &identity.pane).await?;
        ensure!(
            fresh == read && tty_identity(&fs::symlink_metadata(&fresh.tty)?)? == device,
            "Diagnostic process changed before writer initialization"
        );
        self.authorize_terminal_writer(&fresh, catalog).await?;

        let guard = format_all([
            self.paste_guard(&fresh),
            format_equal("pane_input_off", 0),
            format_equal("after-if-shell", ""),
        ]);
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

    async fn prove_paste_buffer_io(
        &self,
        read: &GeometryReadback,
        source: ProbeSource,
    ) -> Result<TtyIdentity> {
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
        let mut snapshot = self.read_custody(read.server).await?;
        for slot in snapshot.iter().flatten() {
            let Some(active) = &slot.active else { continue };
            let (runtime, pane, _, _) = active.identity();
            if runtime == read.target.runtime_id || pane == read.target.pane {
                ensure!(
                    pane != read.target.pane && runtime == read.target.runtime_id,
                    "Original probe custody remains on this pane"
                );
                self.close_replaced_custody(read, slot, active).await?;
            }
        }
        snapshot = self.read_custody(read.server).await?;
        ensure!(
            !snapshot
                .iter()
                .flatten()
                .any(|slot| slot.active.as_ref().is_some_and(|active| {
                    let (runtime, pane, _, _) = active.identity();
                    runtime == read.target.runtime_id || pane == read.target.pane
                })),
            "Probe predecessor custody is unsettled"
        );
        let index = free_custody_index(&snapshot).context("Probe custody slots are full")?;
        let possible = CustodySlot::bind_probe(read, index, source)?;
        self.mutate_custody(read, snapshot[index].as_ref(), &possible, None)
            .await?;
        let active = possible
            .active
            .as_ref()
            .and_then(CustodyBinding::probe)
            .context("Missing probe custody")?;
        let sample = format!("{}\n\t\u{2603}", Uuid::new_v4());
        if let Err(error) = self.load_paste(read, sample.as_bytes(), &possible).await {
            // @invariant: Buffer absence cannot settle an outstanding stdin load.
            let _ = self.close_custody(read, &possible).await;
            return Err(error);
        }
        let completed = possible.complete_probe(read.server)?;
        self.mutate_custody(read, Some(&possible), &completed, None)
            .await?;
        let returned = self
            .geometry_command(&["save-buffer", "-b", &active.buffer, "-"])
            .await;
        self.retire_custody(read, &completed, Some(&active.buffer))
            .await?;
        ensure!(returned? == sample, "Paste sender stdin readback differs");
        Ok(active.device)
    }

    async fn transition_marker(
        &self,
        from: &GeometryReadback,
        next: &str,
    ) -> Result<Option<GeometryReadback>> {
        self.transition_marker_guarded(from, next, self.paste_guard(from))
            .await
    }

    async fn transition_marker_guarded(
        &self,
        from: &GeometryReadback,
        next: &str,
        guard: String,
    ) -> Result<Option<GeometryReadback>> {
        let pane = &from.target.pane;
        let witness = Uuid::new_v4();
        let printed = pane_witness(pane, &witness.to_string());
        let branch = format!("set-option -p -t {pane} {PASTE_OPTION} {next} ; {printed}");
        let rejected = format!("rs-rejected-{witness}");
        let else_branch = rejection_witness(pane, &rejected);
        let reply = self
            .geometry_command(&["if-shell", "-F", "-t", pane, &guard, &branch, &else_branch])
            .await;
        if reply
            .as_ref()
            .is_ok_and(|reply| reply.trim_end() == rejected)
        {
            return Ok(None);
        }
        // @contract: An exact readback can resolve a lost no-effect transition reply.
        self.read_paste_marker(from, next).await.map(Some)
    }

    async fn read_paste_marker(
        &self,
        from: &GeometryReadback,
        expected: &str,
    ) -> Result<GeometryReadback> {
        let after = self
            .geometry_in_state(
                from.target.runtime_id,
                &from.target.pane,
                from.state != PaneState::Live,
            )
            .await?;
        ensure!(
            after.same_pane(from)
                && after.state == from.state
                && after.epoch == from.epoch
                && after.sender_supported
                && after.paste_marker == expected,
            "Paste marker transition unconfirmed"
        );
        Ok(after)
    }

    async fn read_custody(&self, server: ServerGeneration) -> Result<CustodySnapshot> {
        let options: [_; CUSTODY_SLOTS] = std::array::from_fn(|i| custody_option(server, i));
        let guard = format_all([
            generation_guard(server),
            format_equal("after-if-shell", ""),
            format_equal("after-show-options", ""),
            format_equal("after-display-message", ""),
            format_equal("command-error", ""),
        ]);
        let mut branch = String::new();
        for option in &options {
            branch.push_str(&format!("show-options -sq -F '{CUSTODY_ROW}' {option} ; "));
        }
        branch.push_str(&format!("display-message -p -l {CUSTODY_END}"));
        let output = tokio::time::timeout(self.terminal_command_watchdog(), async {
            let mut child = Command::new(&self.tmux)
                .args(["-N", "-S"])
                .arg(&self.socket)
                .args(["if-shell", "-F", &guard, &branch])
                .env_remove("TMUX")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()?;
            let mut output = Vec::new();
            child
                .stdout
                .take()
                .context("Paste custody snapshot unavailable")?
                .take((CUSTODY_SNAPSHOT_LIMIT + 1) as u64)
                .read_to_end(&mut output)
                .await?;
            ensure!(
                output.len() <= CUSTODY_SNAPSHOT_LIMIT,
                "Paste custody snapshot too large"
            );
            ensure!(
                child.wait().await?.success(),
                "Paste custody snapshot failed"
            );
            Ok::<_, anyhow::Error>(output)
        })
        .await
        .context("Paste custody snapshot timed out")??;
        let output = std::str::from_utf8(&output).context("Invalid paste custody snapshot")?;
        let mut rows = output
            .strip_suffix('\n')
            .context("Incomplete paste custody snapshot")?
            .rsplit('\n');
        ensure!(
            rows.next() == Some(CUSTODY_END),
            "Incomplete paste custody snapshot"
        );
        let mut slots: CustodySnapshot = Box::new(std::array::from_fn(|_| None));
        let mut mutations = HashSet::new();
        let mut panes = HashSet::new();
        let mut runtimes = HashSet::new();
        let mut buffers = HashSet::new();
        let mut nonces = HashSet::new();
        for row in rows {
            let (option, value) = row.split_once(' ').context("Invalid paste custody row")?;
            let index = options
                .iter()
                .position(|key| key == option)
                .context("Unknown paste custody slot")?;
            let slot = CustodySlot::parse(server, index, value)?;
            ensure!(
                slots[index].is_none() && mutations.insert(slot.mutation),
                "Duplicate paste custody slot or mutation"
            );
            if let Some(active) = &slot.active {
                let (runtime, pane, nonce, buffer) = active.identity();
                ensure!(
                    runtimes.insert(runtime)
                        && panes.insert(pane.to_owned())
                        && buffers.insert(buffer.to_owned())
                        && nonces.insert(nonce),
                    "Duplicate paste custody binding"
                );
            }
            slots[index] = Some(slot);
        }
        Ok(slots)
    }

    fn custody_guard(&self, read: &GeometryReadback, compare: String) -> String {
        format_all([
            self.paste_guard(read),
            format_equal("after-if-shell", ""),
            compare,
        ])
    }

    async fn mutate_custody(
        &self,
        read: &GeometryReadback,
        previous: Option<&CustodySlot>,
        next: &CustodySlot,
        delete: Option<&str>,
    ) -> Result<()> {
        ensure!(
            read.sender_supported && self.private_sender_socket(),
            "Paste custody mutation unavailable"
        );
        let pane = &read.target.pane;
        let option = custody_option(read.server, next.index);
        let (flag, compare) = match previous {
            Some(previous) => {
                ensure!(
                    previous.index == next.index && previous.mutation != next.mutation,
                    "Invalid paste custody transition"
                );
                ("-s", previous.guard(read.server))
            }
            None => ("-so", format_equal(&option, "")),
        };
        let active = next
            .active
            .as_ref()
            .or_else(|| previous.and_then(|slot| slot.active.as_ref()));
        let origin = active
            .and_then(CustodyBinding::probe)
            .filter(|probe| next.active.is_some() || probe.target.pane == *pane)
            .map(|probe| probe.original_guard(read))
            .transpose()?;
        let checked = |comparison| {
            format_all(
                std::iter::once(self.custody_guard(read, comparison)).chain(origin.iter().cloned()),
            )
        };
        let guard = checked(compare);
        let set = format!("set-option {flag} {option} {}", next.value);
        let branch = match delete {
            Some(buffer) => {
                ensure!(
                    next.active.is_none()
                        && previous
                            .and_then(|slot| slot.active.as_ref())
                            .is_some_and(|active| active.identity().3 == buffer),
                    "Custody retirement deletion does not match its record"
                );
                format!("delete-buffer -b {buffer} ; {set}")
            }
            None => set,
        };
        let claim = next.active.as_ref().filter(|active| {
            !active.completed()
                && previous.is_none_or(|slot| slot.active.is_none())
                && delete.is_none()
        });
        let rejection = claim.map(|_| {
            let prior = previous.map_or("", |slot| slot.value.as_str());
            let changed = checked(format!("#{{!=:#{{{option}}},{prior}}}"));
            let witness = format!("rs-paste-custody-conflict-{}", next.mutation);
            let otherwise = format!(
                "if-shell -F -t {pane} {} {}",
                tmux_word(&changed),
                tmux_word(&pane_witness(pane, &witness))
            );
            (witness, otherwise)
        });
        let mut args = vec!["if-shell", "-F", "-t", pane, &guard, &branch];
        if let Some((_, otherwise)) = &rejection {
            args.push(otherwise);
        }
        let reply = self.geometry_command(&args).await;
        let observed = self.read_custody(read.server).await?;
        let current = observed[next.index].as_ref();
        let rejected = rejection.is_some_and(|(witness, _)| {
            reply
                .as_deref()
                .is_ok_and(|reply| reply.strip_suffix('\n') == Some(witness.as_str()))
        });
        if !rejected && current.is_some_and(|slot| slot.value == next.value) {
            return Ok(());
        }
        if rejected
            && let Some(claim) = claim
            && current.is_some_and(|slot| {
                slot.mutation != next.mutation
                    && previous.is_none_or(|prior| slot.mutation != prior.mutation)
            })
            && !observed.iter().flatten().any(|slot| {
                slot.active.as_ref().is_some_and(|active| {
                    let (runtime, pane, nonce, buffer) = active.identity();
                    let (claimed_runtime, claimed_pane, claimed_nonce, claimed_buffer) =
                        claim.identity();
                    runtime == claimed_runtime
                        || pane == claimed_pane
                        || nonce == claimed_nonce
                        || buffer == claimed_buffer
                })
            })
        {
            return Err(CustodyClaimConflict.into());
        }
        bail!("Paste custody transition is unconfirmed")
    }

    async fn retire_custody(
        &self,
        read: &GeometryReadback,
        slot: &CustodySlot,
        delete: Option<&str>,
    ) -> Result<()> {
        let free = CustodySlot::parse(
            read.server,
            slot.index,
            &format!("v1~{}~free", Uuid::new_v4()),
        )?;
        self.mutate_custody(read, Some(slot), &free, delete).await
    }

    async fn load_paste(
        &self,
        read: &GeometryReadback,
        data: &[u8],
        custody: &CustodySlot,
    ) -> Result<()> {
        ensure!(
            self.private_sender_socket(),
            "Paste load target unavailable"
        );
        let active = custody.active.as_ref().context("Missing load custody")?;
        ensure!(!active.completed(), "Load custody already completed");
        let pane = &read.target.pane;
        let mut guard = self.custody_guard(read, custody.guard(read.server));
        let name = match active {
            CustodyBinding::Payload(payload) => {
                ensure!(
                    payload.matches(read, &payload.buffer) && read.paste_marker == payload.marker,
                    "Payload load custody does not match its pane"
                );
                &payload.buffer.name
            }
            CustodyBinding::Probe(probe) => {
                guard = format_all([guard, probe.original_guard(read)?]);
                &probe.buffer
            }
        };
        let witness = format!("rs-paste-loaded-{}", Uuid::new_v4());
        let printed = pane_witness(pane, &witness);
        let branch = format!("load-buffer -b {name} - ; {printed}");
        tokio::time::timeout(self.terminal_command_watchdog(), async {
            let mut child = Command::new(&self.tmux)
                .args(["-N", "-S"])
                .arg(&self.socket)
                .args(["if-shell", "-F", "-t", pane, &guard, &branch])
                .env_remove("TMUX")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()?;
            child
                .stdin
                .take()
                .context("Paste buffer stdin unavailable")?
                .write_all(data)
                .await?;
            let output = child.wait_with_output().await?;
            ensure!(
                output.status.success()
                    && output.stdout.strip_suffix(b"\n") == Some(witness.as_bytes()),
                "Paste buffer load unconfirmed"
            );
            Ok(())
        })
        .await
        .context("Paste buffer load timed out")?
    }

    async fn delete_paste(
        &self,
        read: &GeometryReadback,
        buffer: &str,
        allow_absent: bool,
    ) -> Result<()> {
        ensure!(
            read.sender_supported && self.private_sender_socket(),
            "Paste cleanup unavailable"
        );
        let pane = &read.target.pane;
        let witness = format!("rs-paste-cleanup-{}", Uuid::new_v4());
        let printed = pane_witness(pane, &witness);
        let guard = format_all([
            self.paste_guard(read),
            format_equal("after-list-buffers", ""),
        ]);
        if allow_absent && !self.paste_buffer_present(pane, &guard, buffer).await? {
            return Ok(());
        }
        let branch = format!("delete-buffer -b {buffer} ; {printed}");
        let reply = self
            .geometry_command(&["if-shell", "-F", "-t", pane, &guard, &branch])
            .await?;
        ensure!(
            reply.trim_end() == witness,
            "Paste buffer deletion unconfirmed"
        );
        Ok(())
    }

    async fn paste_buffer_present(&self, pane: &str, guard: &str, buffer: &str) -> Result<bool> {
        let witness = format!("rs-paste-cleanup-{}", Uuid::new_v4());
        let printed = pane_witness(pane, &witness);
        let filter = format_equal("buffer_name", buffer);
        let branch = format!("list-buffers -F present -f '{filter}' ; {printed}");
        let reply = self
            .geometry_command(&["if-shell", "-F", "-t", pane, guard, &branch])
            .await?;
        let mut lines = reply.lines();
        match (lines.next(), lines.next(), lines.next()) {
            (Some(first), None, None) if first == witness => Ok(false),
            (Some("present"), Some(last), None) if last == witness => Ok(true),
            _ => bail!("Paste buffer absence is unconfirmed"),
        }
    }

    async fn close_custody(&self, read: &GeometryReadback, slot: &CustodySlot) -> Result<()> {
        let active = slot
            .active
            .as_ref()
            .context("Missing active paste custody")?;
        ensure!(
            read.sender_supported && self.private_sender_socket(),
            "Paste cleanup unavailable"
        );
        let (_, _, _, buffer) = active.identity();
        let mut guards = vec![
            self.custody_guard(read, slot.guard(read.server)),
            format_equal("after-list-buffers", ""),
        ];
        if let Some(probe) = active
            .probe()
            .filter(|probe| probe.target.pane == read.target.pane)
        {
            guards.push(probe.original_guard(read)?);
        }
        let guard = format_all(guards);
        let delete = if active.completed() {
            self.paste_buffer_present(&read.target.pane, &guard, buffer)
                .await?
        } else {
            true
        };
        self.retire_custody(read, slot, delete.then_some(buffer))
            .await
    }

    async fn delete_loading_paste(
        &self,
        fenced: &GeometryReadback,
        buffer: &PasteBuffer,
        custody: Option<&CustodySlot>,
    ) -> Result<GeometryReadback> {
        ensure!(
            fenced.sender_supported && self.private_sender_socket(),
            "Paste cleanup unavailable"
        );
        let pane = &fenced.target.pane;
        let retired = buffer.marker(fenced, 'q');
        let branch = format!(
            "delete-buffer -b {} ; set-option -p -t {pane} {PASTE_OPTION} {retired}",
            buffer.name,
        );
        let guard = match custody {
            Some(slot) => self.custody_guard(fenced, slot.guard(fenced.server)),
            None => self.paste_guard(fenced),
        };
        let _ = self
            .geometry_command(&["if-shell", "-F", "-t", pane, &guard, &branch])
            .await;
        self.read_paste_marker(fenced, &retired).await
    }

    async fn prove_paste_replacement(
        &self,
        server: ServerGeneration,
        original_pane: &str,
        current: &GeometryReadback,
    ) -> Result<()> {
        ensure!(
            server == current.server && original_pane != current.target.pane,
            "Original paste server or pane is unconfirmed"
        );
        let output = self
            .geometry_command(&[
                "display-message",
                "-p",
                GENERATION_FORMAT,
                ";",
                "list-panes",
                "-a",
                "-F",
                "#{pane_id}",
            ])
            .await?;
        let mut lines = output.lines();
        ensure!(
            lines.next().and_then(generation) == Some(server),
            "Original paste server changed"
        );
        let mut panes = HashSet::new();
        for pane in lines {
            ensure!(
                canonical_tmux_id(pane, '%') && pane != original_pane && panes.insert(pane),
                "Original paste pane absence is unconfirmed"
            );
        }
        ensure!(
            panes.contains(current.target.pane.as_str()),
            "Current paste cleanup target is unconfirmed"
        );
        if current.nonce.is_empty() {
            let scope = format!(
                "{}|{}|{}|{}|{}",
                server.pid,
                server.start,
                server.token,
                current.target.session,
                current.target.window
            );
            let (pid, exited) = match current.state {
                PaneState::Live => (Some(current.target.pid), false),
                PaneState::Exited(pid) => (pid, true),
            };
            self.corroborate_legacy(&current.target.pane, &current.name, pid, exited, &scope)
                .await?;
        }
        Ok(())
    }

    async fn close_replaced_custody(
        &self,
        current: &GeometryReadback,
        slot: &CustodySlot,
        active: &CustodyBinding,
    ) -> Result<()> {
        let (runtime, pane, _, _) = active.identity();
        ensure!(
            runtime == current.target.runtime_id,
            "Replacement runtime changed"
        );
        let live_probe = active.probe().is_some() && current.state == PaneState::Live;
        let device = live_probe
            .then(|| tty_identity(&fs::symlink_metadata(&current.tty)?))
            .transpose()?;
        self.prove_paste_replacement(current.server, pane, current)
            .await?;
        if active.probe().is_some() {
            let fresh = self
                .geometry_in_state(
                    runtime,
                    &current.target.pane,
                    current.state != PaneState::Live,
                )
                .await?;
            ensure!(
                fresh == *current,
                "Replacement probe cleanup target changed"
            );
            if let Some(device) = device {
                ensure!(
                    process_start(fresh.target.pid)? == fresh.target.process_start
                        && tty_identity(&fs::symlink_metadata(&fresh.tty)?)? == device,
                    "Replacement probe process or TTY changed"
                );
            }
        }
        self.close_custody(current, slot).await
    }

    async fn reclaim_superseded_paste(
        &self,
        scope: &PasteScope,
        current: &GeometryReadback,
    ) -> Result<()> {
        let original = &scope.original;
        ensure!(
            !scope.may_be_same_pane(current),
            "Original paste pane still present"
        );
        self.prove_paste_replacement(original.server, &original.target.pane, current)
            .await?;
        if let Some(buffer) = &scope.buffer {
            let allow_absent = !buffer.requires_deletion(original);
            self.delete_paste(current, &buffer.name, allow_absent)
                .await?;
        } else {
            ensure!(
                original.paste_ready(),
                "Original paste custody is unconfirmed"
            );
        }
        Ok(())
    }

    async fn reconcile_paste(
        &self,
        lane: &mut ResizeLane,
        current: &GeometryReadback,
        snapshot: &CustodySnapshot,
    ) -> bool {
        if lane
            .pending_paste
            .as_ref()
            .is_some_and(|pending| !pending.scope.may_be_same_pane(current))
        {
            let pending = lane.pending_paste.take().expect("checked pending paste");
            self.owners()
                .release(current.target.runtime_id, pending.binding.token);
            lane.retain_paste(pending.scope);
        }
        let mut predecessors_closed = true;
        for slot in snapshot.iter().flatten() {
            let Some(active) = &slot.active else { continue };
            let (runtime, pane, _, _) = active.identity();
            if runtime != current.target.runtime_id || pane == current.target.pane {
                continue;
            }
            if self
                .close_replaced_custody(current, slot, active)
                .await
                .is_err()
            {
                predecessors_closed = false;
            } else if let Some(payload) = active.payload() {
                lane.paste_quarantines.retain(|scope| {
                    scope.original.server != current.server
                        || !scope
                            .buffer
                            .as_ref()
                            .is_some_and(|buffer| payload.matches(&scope.original, buffer))
                });
            }
        }
        let mut index = 0;
        while index < lane.paste_quarantines.len() {
            let scope = &lane.paste_quarantines[index];
            let has_server_record = scope.custody.is_some()
                || snapshot.iter().flatten().any(|slot| {
                    slot.active.as_ref().is_some_and(|active| {
                        scope.original.server == current.server
                            && active.identity().1 == scope.original.target.pane
                    })
                });
            if !has_server_record && self.reclaim_superseded_paste(scope, current).await.is_ok() {
                lane.paste_quarantines.swap_remove(index);
            } else {
                index += 1;
            }
        }
        predecessors_closed
    }

    fn discard_live_rejected_paste(&self, binding: &PasteBinding) {
        let mut owners = self.owners();
        let now = self.now();
        // @security: Under the runtime lane, discard only the live consumed binding without advancing high-water.
        if owners.epoch(binding.terminal.target.runtime_id, binding.token, now)
            == Some(binding.epoch)
            && owners.paste_live(binding, now)
        {
            owners.discard_rejected_stage(binding, now);
        }
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
        let rejected = match self.scoped_geometry(&binding.terminal).await {
            Err(_) => Some(PasteResult::Stale {}),
            Ok(read)
                if self
                    .authorize_terminal_writer(&read, catalog)
                    .await
                    .is_err() =>
            {
                self.discard_live_rejected_paste(&binding);
                return PasteResult::Unavailable {};
            }
            Ok(read) if lane.holds_pane(&read) => Some(PasteResult::Unknown {}),
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

    fn quarantine(
        &self,
        lane: &mut ResizeLane,
        binding: &PasteBinding,
        scope: PasteScope,
    ) -> PasteResult {
        lane.retain_paste(scope);
        self.owners()
            .release(binding.terminal.target.runtime_id, binding.token);
        PasteResult::Unknown {}
    }

    async fn reject_reserved(
        &self,
        lane: &mut ResizeLane,
        binding: &PasteBinding,
        mut scope: PasteScope,
        result: PasteResult,
        catalog: Option<&CatalogLane>,
    ) -> PasteResult {
        let cleaned = async {
            let held = &scope.original;
            self.authorize_terminal_writer(held, catalog).await?;
            let buffer = scope.buffer.as_ref().expect("reserved paste buffer");
            let fenced = self
                .transition_marker(held, &buffer.marker(held, 'q'))
                .await?
                .context("Paste reservation was not fenced")?;
            scope.original = fenced;
            let fenced = &scope.original;
            self.authorize_terminal_writer(fenced, catalog).await?;
            self.close_custody(
                fenced,
                scope.custody.as_ref().context("Missing paste custody")?,
            )
            .await?;
            scope.custody = None;
            self.authorize_terminal_writer(fenced, catalog).await?;
            self.transition_marker(fenced, &fenced.ready_marker(fenced.epoch))
                .await?
                .context("Paste reservation was not retired")
        };
        if cleaned.await.is_ok() {
            return result;
        }
        self.quarantine(lane, binding, scope)
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
        let Ok(read) = self.scoped_geometry(&binding.terminal).await else {
            self.discard_live_rejected_paste(&binding);
            return PasteResult::Stale {};
        };
        if read.epoch != Some(binding.epoch) {
            self.discard_live_rejected_paste(&binding);
            return PasteResult::Stale {};
        }
        if self
            .authorize_terminal_writer(&read, catalog)
            .await
            .is_err()
        {
            self.discard_live_rejected_paste(&binding);
            return PasteResult::Unavailable {};
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
        if lane.holds_pane(&read) {
            return PasteResult::Unknown {};
        }
        match read.paste_admission_hold() {
            Some(PasteAdmission::PriorSendUnknown) => {
                return self.quarantine(&mut lane, &binding, PasteScope::sampled(&read));
            }
            Some(PasteAdmission::Unavailable) => return PasteResult::Unavailable {},
            None => {}
        }
        if !read.input_enabled {
            return PasteResult::Unavailable {};
        }
        let Ok(snapshot) = self.read_custody(read.server).await else {
            return PasteResult::Unavailable {};
        };
        if snapshot.iter().flatten().any(|slot| {
            slot.active
                .as_ref()
                .is_some_and(|active| active.identity().1 == read.target.pane)
        }) {
            return self.quarantine(&mut lane, &binding, PasteScope::sampled(&read));
        }
        if snapshot.iter().flatten().any(|slot| {
            slot.active
                .as_ref()
                .is_some_and(|active| active.identity().0 == runtime)
        }) {
            return PasteResult::Unavailable {};
        }
        let Some(index) = free_custody_index(&snapshot) else {
            return PasteResult::Unavailable {};
        };
        let buffer = PasteBuffer {
            nonce: Uuid::new_v4(),
            operation: binding.operation,
            name: format!("rs-paste-{}", Uuid::new_v4().simple()),
        };
        let Ok(custody) = CustodySlot::bind(&read, index, &buffer, "possible") else {
            return PasteResult::Unavailable {};
        };
        if self
            .authorize_terminal_writer(&read, catalog)
            .await
            .is_err()
        {
            return PasteResult::Unavailable {};
        }
        let loading_marker = buffer.marker(&read, 'l');
        let loading = match self.transition_marker(&read, &loading_marker).await {
            Ok(Some(loading)) => loading,
            Ok(None) => return PasteResult::Stale {},
            Err(_) => {
                let mut original = read;
                original.paste_marker = loading_marker;
                return self.quarantine(
                    &mut lane,
                    &binding,
                    PasteScope {
                        original,
                        buffer: None,
                        custody: None,
                    },
                );
            }
        };
        let mut scope = PasteScope {
            original: loading,
            buffer: Some(buffer),
            custody: Some(custody),
        };
        let buffer = scope.buffer.as_ref().expect("reserved paste buffer");
        let custody = scope.custody.as_ref().expect("reserved paste custody");
        #[cfg(test)]
        PasteBarrier::pause_once(&self.paste_test.custody_claim).await;
        if self
            .authorize_terminal_writer(&scope.original, catalog)
            .await
            .is_err()
        {
            return self.quarantine(&mut lane, &binding, scope);
        }
        if let Err(error) = self
            .mutate_custody(&scope.original, snapshot[index].as_ref(), custody, None)
            .await
        {
            if error.is::<CustodyClaimConflict>() && self.private_sender_socket() {
                if self
                    .authorize_terminal_writer(&scope.original, catalog)
                    .await
                    .is_err()
                {
                    return self.quarantine(&mut lane, &binding, scope);
                }
                scope.custody = None;
                let loading = &scope.original;
                let ready = loading.ready_marker(loading.epoch);
                let guard = format_all([
                    self.paste_guard(loading),
                    format_equal("after-if-shell", ""),
                ]);
                if matches!(
                    self.transition_marker_guarded(loading, &ready, guard).await,
                    Ok(Some(_))
                ) {
                    return PasteResult::Unavailable {};
                }
            }
            return self.quarantine(&mut lane, &binding, scope);
        }
        if self
            .authorize_terminal_writer(&scope.original, catalog)
            .await
            .is_err()
        {
            return self.quarantine(&mut lane, &binding, scope);
        }
        let loaded = self.load_paste(&scope.original, &data, custody).await;
        drop(data);
        if loaded.is_err() {
            return self.quarantine(&mut lane, &binding, scope);
        }
        if self
            .authorize_terminal_writer(&scope.original, catalog)
            .await
            .is_err()
        {
            return self.quarantine(&mut lane, &binding, scope);
        }
        let Ok(completed) = CustodySlot::bind(&scope.original, index, buffer, "completed") else {
            return self.quarantine(&mut lane, &binding, scope);
        };
        if self
            .mutate_custody(&scope.original, Some(custody), &completed, None)
            .await
            .is_err()
        {
            return self.quarantine(&mut lane, &binding, scope);
        }
        let armed = buffer.marker(&scope.original, 'a');
        let spent = buffer.marker(&scope.original, 's');
        scope.custody = Some(completed);
        if self
            .authorize_terminal_writer(&scope.original, catalog)
            .await
            .is_err()
        {
            return self.quarantine(&mut lane, &binding, scope);
        }
        let Ok(Some(held)) = self.transition_marker(&scope.original, &armed).await else {
            return self.quarantine(&mut lane, &binding, scope);
        };
        scope.original = held;
        #[cfg(test)]
        PasteBarrier::pause_once(&self.paste_test.admission).await;
        let current = self.scoped_geometry(&binding.terminal).await.ok();
        if let Some(read) = &current
            && self.authorize_terminal_writer(read, catalog).await.is_err()
        {
            return self.quarantine(&mut lane, &binding, scope);
        }
        let exact = current.as_ref().filter(|read| {
            read.same_pane(&scope.original)
                && read.epoch == scope.original.epoch
                && read.paste_marker == armed
                && read.sender_supported
        });
        let custody = scope.custody.as_ref().expect("completed paste custody");
        let Some(payload) = custody
            .active
            .as_ref()
            .and_then(CustodyBinding::payload)
            .filter(|payload| {
                payload.completed && payload.matches(&scope.original, &payload.buffer)
            })
        else {
            return self.quarantine(&mut lane, &binding, scope);
        };
        let send = self.guarded_paste(&scope.original, &spent, custody, payload);
        // @invariant: The serialized monotonic check is the last admission step before submission.
        let rejection = match exact {
            Some(read) if self.owners().paste_live(&binding, self.now()) => {
                (!read.input_enabled).then_some(PasteResult::Unavailable {})
            }
            _ => Some(PasteResult::Stale {}),
        };
        if let Some(rejection) = rejection {
            return self
                .reject_reserved(&mut lane, &binding, scope, rejection, catalog)
                .await;
        }
        match self.submit_paste(&send).await {
            Ok(reply) if reply.trim_end() == send.rejected => {
                self.reject_reserved(
                    &mut lane,
                    &binding,
                    scope,
                    PasteResult::Unavailable {},
                    catalog,
                )
                .await
            }
            Ok(reply) if reply.trim_end() == send.enqueued => {
                if !self.owners().confirm_paste(&binding) {
                    return self.quarantine(&mut lane, &binding, scope);
                }
                let receipt = Uuid::new_v4();
                scope.original.paste_marker = spent;
                lane.pending_paste = Some(PendingPaste {
                    binding,
                    scope,
                    receipt,
                    released: false,
                });
                PasteResult::Committed { receipt }
            }
            _ => self.quarantine(&mut lane, &binding, scope),
        }
    }

    fn guarded_paste(
        &self,
        held: &GeometryReadback,
        spent: &str,
        custody: &CustodySlot,
        payload: &PayloadCustody,
    ) -> GuardedPaste {
        let buffer = &payload.buffer.name;
        let nonce = payload.buffer.nonce;
        let pane = held.target.pane.clone();
        let guard = format_all([
            self.custody_guard(held, custody.guard(held.server)),
            format_equal("pane_input_off", 0),
        ]);
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
            return if lane
                .paste_quarantines
                .iter()
                .any(|scope| scope.original.target == binding.terminal.target)
            {
                PasteResult::Unknown {}
            } else {
                PasteResult::Stale {}
            };
        };
        if pending.binding != binding || pending.receipt != receipt {
            return PasteResult::Stale {};
        }
        let mut pending = lane.pending_paste.take().expect("matched pending paste");
        let Ok(read) = self.scoped_geometry(&binding.terminal).await else {
            return self.quarantine(&mut lane, &binding, pending.scope);
        };
        if !read.sender_supported
            || !read.same_pane(&pending.scope.original)
            || read.epoch != Some(binding.epoch)
            || read.paste_marker != pending.scope.original.paste_marker
            || self
                .authorize_terminal_writer(&read, catalog)
                .await
                .is_err()
        {
            return self.quarantine(&mut lane, &binding, pending.scope);
        }
        let slot = pending
            .scope
            .custody
            .as_ref()
            .expect("committed paste custody");
        if self.retire_custody(&read, slot, None).await.is_err() {
            return self.quarantine(&mut lane, &binding, pending.scope);
        }
        pending.scope.custody = None;
        if self
            .authorize_terminal_writer(&read, catalog)
            .await
            .is_err()
        {
            return self.quarantine(&mut lane, &binding, pending.scope);
        }
        let ready = read.ready_marker(read.epoch);
        if !matches!(self.transition_marker(&read, &ready).await, Ok(Some(_))) {
            return self.quarantine(&mut lane, &binding, pending.scope);
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
                let runtime = self.find(runtime_id).await?;
                let server = self.terminal_generation(&runtime, catalog).await?;
                self.geometry_for_runtime(&runtime, server).await
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
        let sampled = match first_read.filter(|_| !lane.has_paste_hold()) {
            Some(read) => Ok(read),
            None => read_scope().await,
        };
        let mut read = match sampled {
            Ok(read) => read,
            Err(_) if lane.has_paste_hold() => return Ok(control_hold()),
            Err(error) => return Err(error),
        };
        if let Err(error) = self.authorize_terminal_writer(&read, catalog).await {
            return read.writer_refusal(error);
        }
        if lane.holds_pane(&read) {
            return Ok(control_hold());
        }
        if !read.sender_supported {
            let hold = read
                .paste_admission_hold()
                .unwrap_or(PasteAdmission::Unavailable);
            if hold == PasteAdmission::PriorSendUnknown {
                lane.retain_paste(PasteScope::sampled(&read));
            }
            return Ok(control_recovering(hold));
        }
        let Ok(snapshot) = self.read_custody(read.server).await else {
            return Ok(control_hold());
        };
        if snapshot.iter().flatten().any(|slot| {
            slot.active
                .as_ref()
                .is_some_and(|active| active.identity().1 == read.target.pane)
        }) {
            return Ok(control_hold());
        }
        if lane.has_paste_hold()
            || snapshot.iter().flatten().any(|slot| {
                slot.active
                    .as_ref()
                    .is_some_and(|active| active.identity().0 == runtime_id)
            })
        {
            if terminal.is_some() {
                let current = self
                    .geometry_for_runtime(&self.find(runtime_id).await?, read.server)
                    .await?;
                ensure!(
                    current.same_pane(&read),
                    "Current terminal identity changed"
                );
                if let Err(error) = self.authorize_terminal_writer(&current, catalog).await {
                    return current.writer_refusal(error);
                }
                read = current;
            }
            self.reconcile_paste(&mut lane, &read, &snapshot).await;
            if snapshot.iter().flatten().any(|slot| {
                slot.active
                    .as_ref()
                    .and_then(CustodyBinding::probe)
                    .is_some_and(|probe| probe.target.runtime_id == runtime_id)
            }) {
                return Ok(control_hold());
            }
        }
        if let Some(hold) = read.paste_admission_hold() {
            if hold == PasteAdmission::PriorSendUnknown {
                lane.retain_paste(PasteScope::sampled(&read));
            }
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

    pub(crate) async fn resize_terminal(
        &self,
        terminal: TerminalView,
        token: Uuid,
        sequence: u64,
        size: TerminalSize,
        catalog: Option<&CatalogLane>,
        expected_epoch: Option<ResizeEpoch>,
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
        let Ok(mut lane) = lane.try_lock() else {
            ensure!(expected_epoch.is_none(), "Terminal mutation in progress");
            return Ok(Response::Resize {
                resize: ResizeResult::Busy { terminal, size },
            });
        };
        let read = self.scoped_geometry(&terminal).await?;
        let epoch = read.epoch.context("Engine epoch unavailable")?;
        ensure!(
            expected_epoch.is_none_or(|expected| expected == epoch)
                && !lane.reconcile
                && !lane.holds_pane(&read)
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
        Ok(read)
    }

    async fn prepare_paste_stop(
        &self,
        lane: &mut ResizeLane,
        mut read: GeometryReadback,
        snapshot: &CustodySnapshot,
    ) -> Result<GeometryReadback> {
        ensure!(
            self.reconcile_paste(lane, &read, snapshot).await,
            "Original paste custody is unconfirmed"
        );
        if let Some(probe) = snapshot.iter().flatten().find(|slot| {
            slot.active
                .as_ref()
                .and_then(CustodyBinding::probe)
                .is_some_and(|probe| probe.target.pane == read.target.pane)
        }) {
            self.close_custody(&read, probe).await?;
        }
        let sampled = PasteScope::sampled(&read);
        let slot = snapshot.iter().flatten().find(|slot| {
            slot.active
                .as_ref()
                .and_then(CustodyBinding::payload)
                .is_some_and(|active| active.pane == read.target.pane)
        });
        ensure!(
            sampled.buffer.is_some()
                || read.paste_ready()
                || (read.paste_launch.is_empty() && read.paste_marker.is_empty()),
            "Original paste marker cannot be checked"
        );
        let scopes = lane
            .paste_quarantines
            .iter()
            .chain(lane.pending_paste.iter().map(|pending| &pending.scope));
        for saved in scopes {
            let recovered = PasteBuffer::from_marker(&saved.original);
            let buffer = saved.buffer.as_ref().or(recovered.as_ref());
            ensure!(
                saved.original.same_pane(&read)
                    && saved.original.epoch == read.epoch
                    && buffer.is_some()
                    && buffer == sampled.buffer.as_ref()
                    && (saved.custody.is_none() || slot.is_some()),
                "Original paste buffer identity is unconfirmed"
            );
        }
        let active = slot
            .and_then(|slot| slot.active.as_ref())
            .and_then(CustodyBinding::payload);
        if let Some(active) = active {
            ensure!(
                sampled
                    .buffer
                    .as_ref()
                    .is_some_and(|buffer| active.matches(&read, buffer)),
                "Original paste custody does not match its pane marker"
            );
        }
        let Some(buffer) = sampled.buffer else {
            return Ok(read);
        };
        lane.retain_paste(PasteScope::sampled(&read));
        ensure!(
            read.sender_supported && self.private_sender_socket(),
            "Paste cleanup unavailable"
        );
        let requires_deletion = match active {
            Some(active) => !active.completed && read.paste_marker != buffer.marker(&read, 'q'),
            None => buffer.requires_deletion(&read),
        };
        let fenced = buffer.marker(&read, if requires_deletion { 'f' } else { 'q' });
        if read.paste_marker != fenced {
            read = self
                .transition_marker(&read, &fenced)
                .await?
                .context("Original paste sender was not fenced")?;
        }
        if requires_deletion {
            read = self.delete_loading_paste(&read, &buffer, slot).await?;
        }
        match (slot, active) {
            (Some(slot), Some(active)) if active.completed => {
                self.close_custody(&read, slot).await?
            }
            (Some(slot), _) => self.retire_custody(&read, slot, None).await?,
            (None, _) if !requires_deletion => self.delete_paste(&read, &buffer.name, true).await?,
            (None, _) => {}
        }
        if let Some(pending) = lane.pending_paste.as_mut() {
            pending.scope.custody = None;
        }
        for scope in &mut lane.paste_quarantines {
            scope.custody = None;
        }
        Ok(read)
    }

    async fn stop_runtime(
        &self,
        runtime_id: Uuid,
        expected_pane_id: &str,
        catalog: Option<&CatalogLane>,
    ) -> Result<Response> {
        ensure!(
            canonical_tmux_id(expected_pane_id, '%'),
            "Invalid terminal pane ID"
        );
        let lock = self.lane(runtime_id);
        let mut lane = lock.lock().await;
        let runtime = self.find(runtime_id).await?;
        let pane = &runtime.pane_id;
        ensure!(pane == expected_pane_id, "runtime precondition failed");
        let server = self.terminal_generation(&runtime, catalog).await?;
        let read = if runtime.exited {
            self.retained_geometry(&runtime, server).await?
        } else {
            self.geometry_for_runtime(&runtime, server).await?
        };
        let authorize = async |read: &GeometryReadback, settle| {
            if catalog.is_none() && runtime.exited && read.diagnostic.is_none() {
                if let Some((nonce, _)) = read.nonce.split_once(':') {
                    self.corroborate_terminal(&read.identity(), nonce).await?;
                    return Ok(None);
                }
                self.prove_legacy_terminal(&read.identity()).await?;
            }
            self.authorize_terminal_mutation(read, catalog, settle)
                .await
        };
        authorize(&read, true).await?;
        let snapshot = self.read_custody(server).await?;
        authorize(&read, false).await?;
        let checked = self.prepare_paste_stop(&mut lane, read, &snapshot).await?;
        let birth = authorize(&checked, false).await?;
        let guard = format_all(
            [
                checked.guard(checked.epoch),
                format_equal(PASTE_LAUNCH_OPTION, checked.launch()),
                format_equal(PASTE_OPTION, &checked.paste_marker),
                format_equal("after-if-shell", ""),
                format_equal("after-kill-session", ""),
            ]
            .into_iter()
            .chain(birth.as_ref().map(ManagedProvenance::guard)),
        );
        let witness = format!("rs-stopped-{}", Uuid::new_v4());
        let printed = pane_witness(pane, &witness);
        let branch = format!("{printed} ; kill-session -t ={}", runtime.tmux_name);
        self.owners().revoke(runtime_id);
        let reply = self
            .command(&["if-shell", "-F", "-t", pane, &guard, &branch])
            .await?;
        ensure!(
            reply.trim_end() == witness,
            "Stop target changed or its reply was lost"
        );
        if lane
            .pending_paste
            .as_ref()
            .is_some_and(|pending| pending.scope.original.same_pane(&checked))
        {
            lane.pending_paste = None;
        }
        lane.paste_quarantines
            .retain(|scope| !scope.original.same_pane(&checked));
        lane.confirmed = None;
        lane.reconcile = false;
        Ok(Response::Stopped { runtime_id })
    }

    async fn prove_cli_legacy(&self, runtime: &Runtime) -> Result<()> {
        let output = self.geometry_output(&runtime.pane_id).await?;
        let fields = Self::geometry_fields(&output)?;
        ensure!(
            fields[5] == runtime.pane_id
                && fields[7] == runtime.tmux_name
                && fields[10].is_empty()
                && fields[11] == if runtime.exited { "1" } else { "0" },
            "CLI terminal provenance is unconfirmed"
        );
        self.corroborate_legacy(
            &runtime.pane_id,
            &runtime.tmux_name,
            runtime.pane_pid,
            runtime.exited,
            &fields[..5].join("|"),
        )
        .await
    }

    async fn cli_observation(
        &self,
        runtime: &Runtime,
        catalog: &CatalogLane,
    ) -> Result<Option<(GeometryReadback, Option<ManagedProvenance>)>> {
        let id = runtime.runtime_id;
        let records = Self::runtime_catalog(id, runtime.session_id, catalog)
            .await
            .map_err(|issue| anyhow::anyhow!("Managed terminal catalog unavailable: {issue:?}"))?;
        let server = if let Some(operation) = &records.managed {
            operation.generation
        } else if let Some(mark) = &records.diagnostic {
            mark.generation
        } else {
            self.prove_cli_legacy(runtime).await?;
            return Ok(None);
        };
        let read = if runtime.exited {
            self.retained_geometry(runtime, server).await?
        } else {
            self.geometry_for_runtime(runtime, server).await?
        };
        let birth = self
            .observe_terminal_records(&read.identity(), Ok(records), false)
            .await?;
        Ok(Some((read, birth)))
    }

    async fn create_diagnostic(
        &self,
        catalog: &CatalogLane,
        session_id: Uuid,
        runtime_id: Uuid,
        cwd: &str,
        argv: &[String],
        label: &str,
    ) -> Result<(Runtime, PaneIdentity, ServerGeneration, Option<String>)> {
        ensure!(
            !session_id.is_nil() && !runtime_id.is_nil(),
            "Invalid diagnostic IDs"
        );
        let sample = self.sample_generation().await?;
        let capacity_error = |issue| anyhow::anyhow!("Creation capacity unavailable: {issue:?}");
        let lookup_error = |issue| match sample {
            GenerationSample::Unstamped { .. } => {
                anyhow::anyhow!("Terminal generation unavailable: {issue:?}")
            }
            GenerationSample::Stamped(_) => capacity_error(issue),
        };
        if let Some(mark) = catalog
            .run(move |db, _| db.diagnostic(runtime_id, session_id))
            .await
            .map_err(lookup_error)?
        {
            ensure!(
                (mark.session_id, mark.runtime_id) == (session_id, runtime_id),
                "session/runtime already exists; reconcile instead of retrying creation"
            );
            let evidence = self.observe_diagnostic(&mark).await?;
            let runtime = self.find(runtime_id).await?;
            ensure!(
                runtime.tmux_name == mark.target
                    && runtime.pane_id == evidence.identity.pane
                    && runtime.pane_pid == evidence.pane_pid
                    && runtime.exited == evidence.exited,
                "Diagnostic creation target changed"
            );
            let pane = evidence.identity;
            let generation = mark.generation;
            Self::settle_diagnostic(catalog, mark, pane.clone()).await?;
            return Ok((runtime, pane, generation, None));
        }
        let generation = sample.candidate();
        let targets = self
            .creation_targets(catalog, generation)
            .await
            .map_err(capacity_error)?;
        Self::check_diagnostic_capacity(&targets, session_id, runtime_id)?;

        let mark = DiagnosticCreate {
            session_id,
            runtime_id,
            target: tmux_name(session_id, runtime_id),
            nonce: Uuid::new_v4(),
            generation,
            pane: None,
        };
        let saved = mark.clone();
        let marked = catalog
            .run(move |db, _| db.mark_diagnostic(&saved))
            .await
            .map_err(|issue| anyhow::anyhow!("Diagnostic reservation unavailable: {issue:?}"))?;
        ensure!(
            marked,
            "session/runtime already exists; reconcile instead of retrying creation"
        );
        self.establish_generation(generation).await?;

        let name = &mark.target;
        let nonce = mark.nonce;
        let command = argv.iter().map(|arg| tmux_word(arg)).collect::<Vec<_>>();
        let branch = format!(
            "new-session -d -P -F '{RECEIPT_FORMAT}' -s {name} -c {} -x 100 -y 30 \
             -- /usr/bin/env {} ; set-option -Fpo -t ={name}:0.0 {DIAGNOSTIC_OPTION} \
             '{nonce}:#{{session_id}}:#{{window_id}}:#{{pane_id}}:#{{pane_pid}}' ; \
             set-option -t ={name}:0.0 @rs_command {}",
            tmux_word(&cwd.replace('#', "##")),
            command.join(" "),
            tmux_word(label),
        );
        let receipt = self
            .command(&["if-shell", "-F", &creation_guard(generation), &branch])
            .await?;
        let pane = pane_identity(&receipt.trim_end().split('|').collect::<Vec<_>>())
            .context("Diagnostic creation could not be confirmed in the admitted generation")?;
        let evidence = self.observe_diagnostic(&mark).await?;
        ensure!(
            evidence.identity == pane,
            "Diagnostic creation target changed"
        );
        let runtime = Self::diagnostic_runtime(&mark, &evidence, label);
        let stamp = format!(
            "{nonce}:{}:{}:{}:{}",
            pane.session, pane.window, pane.pane, pane.pid
        );
        Self::settle_diagnostic(catalog, mark, pane.clone()).await?;
        Ok((runtime, pane, generation, Some(stamp)))
    }

    fn diagnostic_runtime(
        mark: &DiagnosticCreate,
        evidence: &PaneEvidence,
        label: &str,
    ) -> Runtime {
        Runtime {
            session_id: mark.session_id,
            runtime_id: mark.runtime_id,
            tmux_name: mark.target.clone(),
            pane_id: evidence.identity.pane.clone(),
            command: label.into(),
            pane_pid: evidence.pane_pid,
            exited: evidence.exited,
            width: evidence.width,
            height: evidence.height,
            origin: RuntimeOrigin::Legacy,
            workspace_id: None,
            operation_id: None,
        }
    }

    fn check_diagnostic_capacity(
        targets: &HashSet<(Uuid, Uuid)>,
        session_id: Uuid,
        runtime_id: Uuid,
    ) -> Result<()> {
        ensure!(
            targets.len() < RUNTIME_LIMIT,
            "local spike limit is 16 runtimes"
        );
        ensure!(
            !targets
                .iter()
                .any(|(session, runtime)| *runtime == runtime_id || *session == session_id),
            "session/runtime already exists; reconcile instead of retrying creation"
        );
        Ok(())
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
                let lock = self.lane(runtime_id);
                let mut lane = lock.lock().await;
                let cwd = cwd.canonicalize()?;
                let cwd = cwd.to_str().context("cwd must be UTF8")?;
                let label = Path::new(&argv[0])
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("CLI");
                let (runtime, identity, server, stamp) = if let Some(catalog) = catalog {
                    let (runtime, identity, generation, stamp) = self
                        .create_diagnostic(catalog, session_id, runtime_id, cwd, &argv, label)
                        .await?;
                    if stamp.is_none() {
                        return Ok(Response::Created { runtime });
                    }
                    (runtime, Some(identity), generation, stamp)
                } else {
                    let targets = self
                        .list()
                        .await?
                        .into_iter()
                        .map(|runtime| (runtime.session_id, runtime.runtime_id))
                        .collect();
                    Self::check_diagnostic_capacity(&targets, session_id, runtime_id)?;
                    let generation = self.sample_generation().await?.candidate();
                    self.establish_generation(generation).await?;
                    let name = tmux_name(session_id, runtime_id);
                    let target = format!("={name}:0.0");
                    let mut args = vec![
                        "new-session",
                        "-d",
                        "-P",
                        "-F",
                        RECEIPT_FORMAT,
                        "-s",
                        &name,
                        "-c",
                        cwd,
                        "-x",
                        "100",
                        "-y",
                        "30",
                        "--",
                        "/usr/bin/env",
                    ];
                    args.extend(argv.iter().map(String::as_str));
                    let receipt = self.command(&args).await?;
                    self.command(&["set-option", "-t", &target, "@rs_command", label])
                        .await?;
                    let identity =
                        pane_identity(&receipt.trim_end().split('|').collect::<Vec<_>>());
                    (self.find(runtime_id).await?, identity, generation, None)
                };
                self.owners().revoke(runtime_id);
                if let Some(identity) = identity {
                    let _ = self
                        .stamp_paste(&runtime, server, &identity, stamp.as_deref(), catalog)
                        .await;
                    if lane.has_paste_hold()
                        && let Ok(current) = self.geometry_for_runtime(&runtime, server).await
                        && current.server == server
                        && current.target.session == identity.session
                        && current.target.window == identity.window
                        && current.target.pane == identity.pane
                        && current.target.pid == identity.pid
                        && current.nonce.is_empty()
                        && current.diagnostic == stamp
                        && let Ok(snapshot) = self.read_custody(server).await
                        && self
                            .authorize_terminal_writer(&current, catalog)
                            .await
                            .is_ok()
                    {
                        self.reconcile_paste(&mut lane, &current, &snapshot).await;
                    }
                }
                Ok(Response::Created { runtime })
            }
            Request::Snapshot { runtime_id } => {
                let runtime = self.find(runtime_id).await?;
                let checked = match catalog {
                    Some(catalog) => self.cli_observation(&runtime, catalog).await?,
                    None => {
                        self.prove_cli_legacy(&runtime).await?;
                        None
                    }
                };
                let text = if let Some((read, birth)) = checked {
                    let pane = &read.target.pane;
                    let witness = format!("rs-captured-{}", Uuid::new_v4());
                    let printed = pane_witness(pane, &witness);
                    let branch = format!("capture-pane -p -t {pane} -S -100 ; {printed}");
                    let mut guards = vec![read.guard(read.epoch)];
                    if let Some(birth) = birth {
                        guards.push(birth.guard());
                    }
                    let guard = format_all(guards);
                    let reply = self
                        .command(&["if-shell", "-F", "-t", pane, &guard, &branch])
                        .await?;
                    reply
                        .strip_suffix(&format!("{witness}\n"))
                        .context("Snapshot target changed")?
                        .to_owned()
                } else {
                    let target = format!("={}:0.0", runtime.tmux_name);
                    self.command(&["capture-pane", "-p", "-t", &target, "-S", "-100"])
                        .await?
                };
                Ok(Response::Snapshot { text })
            }
            Request::Attach { runtime_id } => {
                let runtime = self.find(runtime_id).await?;
                ensure!(
                    !runtime.exited,
                    "runtime has exited; no automatic resurrection"
                );
                if let Some(catalog) = catalog {
                    self.cli_observation(&runtime, catalog).await?;
                } else {
                    self.prove_cli_legacy(&runtime).await?;
                }
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
                let runtime = self.find(runtime_id).await?;
                let server = self
                    .terminal_generation(&runtime, catalog)
                    .await
                    .with_context(|| {
                        format!("Terminal observation unavailable for runtime {runtime_id}")
                    })?;
                let read = self.geometry_for_runtime(&runtime, server).await?;
                let proof = self.observe_terminal(&read.identity(), catalog).await?;
                let terminal = TerminalView {
                    target: read.target.clone(),
                    agent_epoch: self.agent_epoch,
                    view_id,
                };
                let sample = self.sample_provider(&read).await.ok();
                self.remember_historical_view(&terminal, proof.as_ref());
                Ok(Response::TerminalOpened {
                    status: read.status(terminal, sample),
                })
            }
            Request::AcquireTerminal { terminal } => {
                self.acquire_terminal(terminal.target.runtime_id, Some(terminal), catalog)
                    .await
            }
            Request::TerminalStatus { terminal } => {
                match self
                    .observe_terminal_status(terminal.clone(), catalog)
                    .await
                {
                    Ok(status) => Ok(Response::TerminalStatus { status }),
                    Err(TerminalStatusFailure::Changed) => {
                        let mut views = self.historical_views.lock().expect("historical view lock");
                        if views
                            .get(&terminal.view_id)
                            .is_some_and(|issued| issued.terminal == terminal)
                        {
                            views.remove(&terminal.view_id);
                        }
                        Ok(Response::TerminalChanged {})
                    }
                    Err(TerminalStatusFailure::Unavailable(error)) => Err(error),
                }
            }
            Request::ResizeTerminal {
                terminal,
                token,
                sequence,
                size,
            } => {
                self.resize_terminal(terminal, token, sequence, size, catalog, None)
                    .await
            }
            Request::Renew { runtime_id, token } => {
                let lock = self
                    .existing_lane(runtime_id)
                    .context("Керування втрачено")?;
                let lane = lock.lock().await;
                let checked = self.writer_geometry(&lane, runtime_id, token).await;
                if let Err(error) = checked {
                    self.owners().release(runtime_id, token);
                    return Err(error);
                }
                let read = checked?;
                if let Err(error) = self.authorize_terminal_writer(&read, catalog).await {
                    self.owners().release(runtime_id, token);
                    return read.writer_refusal(error);
                }
                if lane.holds_pane(&read) {
                    return Ok(control_hold());
                }
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
                let read = self.writer_geometry(&lane, runtime_id, token).await?;
                if let Err(error) = self.authorize_terminal_writer(&read, catalog).await {
                    return read.writer_refusal(error);
                }
                if lane.holds_pane(&read) {
                    return Ok(control_hold());
                }
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
                self.stop_runtime(runtime_id, &expected_pane_id, catalog)
                    .await
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

    #[tokio::test]
    async fn w1_terminal_paste_custody_tombstone_reuse_fences_stale_claim_and_clear() {
        let f = Fixture::paste();
        let mut first = writer(&f).await;
        let (other, _process) = f.second_pane(false);
        let (token, epoch) = f.acquire(other.clone()).await;
        let other = PasteBinding {
            terminal: other,
            token,
            epoch,
            ..first.clone()
        };
        let server = f.engine.generation().await.unwrap();
        set(&f, "custody-write", "lost");
        stage(&f, &first, DRAFT).await;
        let first_receipt = receipt(paste_reply(&f.engine, commit(&first, DRAFT.len())).await);
        set(&f, "custody-write", "");
        stage(&f, &other, DRAFT).await;
        let other_receipt = receipt(paste_reply(&f.engine, commit(&other, DRAFT.len())).await);
        let occupied = f.engine.read_custody(server).await.unwrap();
        let old = occupied[0].as_ref().unwrap();
        let neighbor = occupied[1].as_ref().unwrap();
        let old_binding = old
            .active
            .as_ref()
            .and_then(CustodyBinding::payload)
            .unwrap();
        assert!(
            old_binding.completed
                && neighbor
                    .active
                    .as_ref()
                    .and_then(CustodyBinding::payload)
                    .unwrap()
                    .completed
        );
        assert_eq!(
            paste_reply(&f.engine, ack(&first, first_receipt)).await,
            PasteResult::Acknowledged { writable: true }
        );
        let retired = f.engine.read_custody(server).await.unwrap();
        let free = retired[0].as_ref().unwrap();
        assert!(free.active.is_none());
        assert_ne!(old.mutation, free.mutation);
        assert_eq!(retired[1].as_ref().unwrap().value, neighbor.value);

        first.operation += 1;
        first.sequence += 1;
        stage(&f, &first, DRAFT).await;
        let next_receipt = receipt(paste_reply(&f.engine, commit(&first, DRAFT.len())).await);
        let reused = f.engine.read_custody(server).await.unwrap();
        let next = reused[0].as_ref().unwrap();
        assert!(
            next.active
                .as_ref()
                .and_then(CustodyBinding::payload)
                .unwrap()
                .completed
        );
        assert_ne!(next.mutation, free.mutation);
        assert_ne!(
            next.active
                .as_ref()
                .and_then(CustodyBinding::payload)
                .unwrap()
                .buffer
                .name,
            old_binding.buffer.name
        );
        let read = f.engine.scoped_geometry(&first.terminal).await.unwrap();
        let stale_claim = CustodySlot::bind(&read, 0, &old_binding.buffer, "possible").unwrap();
        let writes = file(&f, "custody-writes");
        assert!(
            f.engine
                .mutate_custody(&read, None, &stale_claim, None)
                .await
                .is_err()
        );
        assert!(
            f.engine
                .mutate_custody(&read, Some(free), &stale_claim, None)
                .await
                .is_err()
        );
        assert!(f.engine.retire_custody(&read, old, None).await.is_err());
        let mut wrong_target = read.clone();
        wrong_target.target.window = "@99".into();
        assert!(
            f.engine
                .retire_custody(&wrong_target, next, None)
                .await
                .is_err()
        );
        assert_eq!(file(&f, "custody-writes"), writes);
        let unchanged = f.engine.read_custody(server).await.unwrap();
        assert_eq!(unchanged[0].as_ref().unwrap().value, next.value);
        assert_eq!(unchanged[1].as_ref().unwrap().value, neighbor.value);

        assert_eq!(
            paste_reply(&f.engine, ack(&first, next_receipt)).await,
            PasteResult::Acknowledged { writable: true }
        );
        assert_eq!(
            paste_reply(&f.engine, ack(&other, other_receipt)).await,
            PasteResult::Acknowledged { writable: true }
        );
        let settled = f.engine.read_custody(server).await.unwrap();
        assert!(settled.iter().flatten().all(|slot| slot.active.is_none()));
        assert_eq!(settled.iter().flatten().count(), 2);
        assert_eq!(file(&f, "pane0-bytes"), DRAFT.repeat(2));
        assert_eq!(file(&f, "pane1-bytes"), DRAFT);
        assert_eq!(file(&f, "submissions"), b"111");
        assert_eq!(file(&f, "effects"), b"111");
        assert!(file(&f, "deletions").is_empty());
    }

    #[tokio::test]
    async fn w1_terminal_paste_custody_refuses_unproved_or_full_snapshot() {
        for mode in [
            "missing",
            "malformed",
            "duplicate-row",
            "duplicate-binding",
            "oversized",
            "over-budget",
            "foreign",
            "full",
        ] {
            let f = Fixture::paste();
            let binding = writer(&f).await;
            let mut read = f.engine.scoped_geometry(&binding.terminal).await.unwrap();
            let server = read.server;
            fs::create_dir(f.temp.path().join("custody")).unwrap();
            let put = |index, value: &str| {
                set(
                    &f,
                    &format!("custody/{}", custody_option(server, index)),
                    value,
                )
            };
            let bound = || PasteBuffer {
                nonce: Uuid::new_v4(),
                operation: 1,
                name: format!("rs-paste-{}", Uuid::new_v4().simple()),
            };
            match mode {
                "missing" | "over-budget" => set(&f, "custody-snapshot", mode),
                "malformed" => put(0, "v2~bad~free"),
                "duplicate-row" => {
                    set(&f, "custody-snapshot", mode);
                    put(0, &format!("v1~{}~free", Uuid::new_v4()));
                }
                "duplicate-binding" => {
                    let buffer = bound();
                    for i in 0..2 {
                        put(
                            i,
                            &CustodySlot::bind(&read, i, &buffer, "possible")
                                .unwrap()
                                .value,
                        );
                    }
                }
                "oversized" => put(0, &"a".repeat(CUSTODY_VALUE_LIMIT + 1)),
                "foreign" => {
                    let value = CustodySlot::bind(&read, 0, &bound(), "possible")
                        .unwrap()
                        .value;
                    put(
                        0,
                        &value.replace(&server.token.to_string(), &Uuid::new_v4().to_string()),
                    );
                }
                "full" => {
                    for i in 0..CUSTODY_SLOTS {
                        read.target.pane = format!("%{}", i + 1);
                        read.target.runtime_id = Uuid::new_v4();
                        read.name = tmux_name(Uuid::new_v4(), read.target.runtime_id);
                        put(
                            i,
                            &CustodySlot::bind(&read, i, &bound(), "possible")
                                .unwrap()
                                .value,
                        );
                    }
                }
                _ => unreachable!(),
            }
            let marker = file(&f, "marker");
            assert_eq!(
                f.engine.read_custody(server).await.is_ok(),
                mode == "full",
                "{mode}"
            );
            stage(&f, &binding, DRAFT).await;
            assert_eq!(
                paste_reply(&f.engine, commit(&binding, DRAFT.len())).await,
                PasteResult::Unavailable {},
                "{mode}"
            );
            assert_eq!(file(&f, "marker"), marker, "{mode}");
            for effect in [
                "last-load",
                "submissions",
                "effects",
                "deletions",
                "custody-writes",
            ] {
                assert!(file(&f, effect).is_empty(), "{mode}: {effect}");
            }
            if mode != "full" {
                f.engine.handle(release(&binding), None).await.unwrap();
                assert!(
                    matches!(
                        f.engine
                            .handle(
                                Request::AcquireTerminal {
                                    terminal: binding.terminal
                                },
                                None
                            )
                            .await
                            .unwrap(),
                        Response::ControlRecovering {
                            paste_admission: PasteAdmission::PriorSendUnknown
                        }
                    ),
                    "{mode}"
                );
            }
        }
    }

    #[tokio::test]
    async fn w1_terminal_paste_custody_unconfirmed_reservation_never_loads() {
        for (control, outcome) in [
            ("custody-write", "before"),
            ("custody-snapshot", "hide-write"),
        ] {
            let f = Fixture::paste();
            let binding = writer(&f).await;
            set(&f, control, outcome);
            stage(&f, &binding, DRAFT).await;
            assert_eq!(
                paste_reply(&f.engine, commit(&binding, DRAFT.len())).await,
                PasteResult::Unknown {},
                "{outcome}"
            );
            assert_held(&f.engine, &binding).await;
            for effect in [
                "last-load",
                "paste-load-attempts",
                "paste-loads",
                "submissions",
                "effects",
                "deletions",
                "custody-conflicts",
            ] {
                assert!(file(&f, effect).is_empty(), "{outcome}: {effect}");
            }
            set(&f, control, "");
            let snapshot = f
                .engine
                .read_custody(f.engine.generation().await.unwrap())
                .await
                .unwrap();
            assert_eq!(
                snapshot.iter().flatten().count(),
                usize::from(outcome == "hide-write")
            );
            assert!(
                f.engine
                    .handle(
                        Request::Stop {
                            runtime_id: binding.terminal.target.runtime_id,
                            expected_pane_id: binding.terminal.target.pane.clone(),
                        },
                        None,
                    )
                    .await
                    .is_err(),
                "{outcome}"
            );
            assert_held(&f.engine, &binding).await;
            for effect in [
                "paste-load-attempts",
                "paste-loads",
                "submissions",
                "effects",
                "deletions",
                "removed-0",
            ] {
                assert!(file(&f, effect).is_empty(), "{outcome}: {effect}");
            }
        }
    }

    #[tokio::test]
    async fn w1_resize_custody_requires_positive_deletion_only_for_possible_late_load() {
        for completed in [false, true] {
            let f = Fixture::paste();
            let binding = writer(&f).await;
            let runtime_id = binding.terminal.target.runtime_id;
            if completed {
                set(&f, "paste-mode", "lost-before");
            } else {
                set(&f, "load-mode", "before");
            }
            stage(&f, &binding, DRAFT).await;
            assert_eq!(
                paste_reply(&f.engine, commit(&binding, DRAFT.len())).await,
                PasteResult::Unknown {}
            );
            let server = f.engine.generation().await.unwrap();
            let reserved = f.engine.read_custody(server).await.unwrap();
            let slot = reserved[0].as_ref().unwrap();
            let active = slot
                .active
                .as_ref()
                .and_then(CustodyBinding::payload)
                .unwrap();
            assert_eq!(active.completed, completed);
            let buffer = &active.buffer.name;
            if completed {
                assert_eq!(file(&f, buffer), DRAFT);
                fs::remove_file(f.temp.path().join(buffer)).unwrap();
            }
            assert!(!f.temp.path().join(buffer).exists());

            let (replacement, _process) = f.second_pane(true);
            set(&f, "removed-0", "1");
            let (engine, _) = f.restart();
            let stop = || Request::Stop {
                runtime_id,
                expected_pane_id: replacement.target.pane.clone(),
            };
            if !completed {
                assert!(engine.handle(stop(), None).await.is_err());
                assert!(file(&f, "removed-1").is_empty());
                assert!(file(&f, "deletions").is_empty());
                let observed = engine.read_custody(server).await.unwrap();
                assert_eq!(observed[0].as_ref().unwrap().value, slot.value);
                fs::write(f.temp.path().join(buffer), DRAFT).unwrap();
            }
            assert!(matches!(engine.handle(stop(), None).await.unwrap(),
                Response::Stopped { runtime_id: stopped } if stopped == runtime_id));
            let settled = engine.read_custody(server).await.unwrap();
            let free = settled[0].as_ref().unwrap();
            assert!(free.active.is_none());
            assert_ne!(free.mutation, slot.mutation);
            assert!(!f.temp.path().join(buffer).exists());
            if completed {
                assert!(file(&f, "deletions").is_empty());
                assert_eq!(file(&f, "submissions"), b"1");
            } else {
                assert_eq!(text(&f, "deletions"), format!("{buffer}\n"));
                assert!(file(&f, "submissions").is_empty());
            }
            for effect in ["effects", "pane0-bytes", "pane1-bytes"] {
                assert!(file(&f, effect).is_empty(), "{effect}");
            }
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
        let f = Fixture::new();
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
                .stamp_paste(&runtime, read.server, &pane, None, None)
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
                        token: Uuid::new_v4(),
                        ..b.clone()
                    },
                    PasteBinding {
                        epoch: ResizeEpoch {
                            owner: Uuid::new_v4(),
                            ..b.epoch
                        },
                        ..b.clone()
                    },
                    PasteBinding {
                        terminal: TerminalView {
                            view_id: Uuid::new_v4(),
                            ..b.terminal.clone()
                        },
                        ..b.clone()
                    },
                    PasteBinding {
                        sequence: b.sequence + 1,
                        ..b.clone()
                    },
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
    async fn w1_terminal_paste_replacement_scope() {
        for (native, restart, committed) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (true, true, false),
            (true, false, true),
        ] {
            let f = Fixture::paste();
            let old = writer(&f).await;
            stage(&f, &old, DRAFT).await;
            if !committed {
                set(&f, "paste-mode", "lost-before");
            }
            let result = paste_reply(&f.engine, commit(&old, DRAFT.len())).await;
            let old_receipt = if committed {
                Some(receipt(result))
            } else {
                assert_eq!(result, PasteResult::Unknown {});
                None
            };
            let (restarted, terminal) = f.restart();
            let (engine, terminal) = if restart {
                (&restarted, terminal)
            } else {
                (&*f.engine, old.terminal.clone())
            };
            assert_held(
                engine,
                &PasteBinding {
                    terminal,
                    ..old.clone()
                },
            )
            .await;
            let old_bytes = if committed { DRAFT } else { b"" };
            assert_eq!(file(&f, "pane0-bytes"), old_bytes);
            let (mut successor, _process) = f.second_pane(true);
            successor.agent_epoch = engine.agent_epoch;
            assert_ne!(old.terminal.target.pid, successor.target.pid);
            assert_eq!(old.terminal.target.runtime_id, successor.target.runtime_id);
            set(&f, "removed-0", "1");
            set(&f, "absence-unconfirmed", "1");
            let request = if native {
                Request::AcquireTerminal {
                    terminal: successor.clone(),
                }
            } else {
                Request::Acquire {
                    runtime_id: successor.target.runtime_id,
                }
            };
            let token = match engine.handle(request, None).await.unwrap() {
                Response::TerminalControl { token, .. } if native => token,
                Response::Control { token } if !native => token,
                response => panic!("Expected a checked successor grant, got {response:?}"),
            };
            assert_ne!(token, old.token);
            assert!(engine.handle(ordinary(&old, 0), None).await.is_err());
            assert_eq!(
                paste_reply(engine, commit(&old, DRAFT.len())).await,
                PasteResult::Stale {}
            );
            if let Some(receipt) = old_receipt {
                assert_ne!(
                    paste_reply(engine, ack(&old, receipt)).await,
                    PasteResult::Acknowledged { writable: true }
                );
            }
            let new = PasteBinding {
                terminal: successor,
                token,
                ..old
            };
            assert!(matches!(
                engine.handle(ordinary(&new, 0), None).await.unwrap(),
                Response::Accepted {}
            ));
            assert_eq!(file(&f, "pane0-bytes"), old_bytes);
            assert_eq!(file(&f, "pane1-bytes"), b"K");
            assert_eq!(file(&f, "submissions"), b"1");
            let expected_effects: &[u8] = if committed { b"1" } else { b"" };
            assert_eq!(file(&f, "effects"), expected_effects);
            assert!(file(&f, "deletions").is_empty());
            if !committed {
                let buffer = text(&f, "last-load");
                assert_eq!(file(&f, &buffer), DRAFT);
                set(&f, "absence-unconfirmed", "");
                engine.handle(release(&new), None).await.unwrap();
                assert!(matches!(
                    engine
                        .handle(
                            Request::AcquireTerminal {
                                terminal: new.terminal,
                            },
                            None,
                        )
                        .await
                        .unwrap(),
                    Response::TerminalControl { .. }
                ));
                assert!(!f.temp.path().join(&buffer).exists());
                assert_eq!(text(&f, "deletions"), format!("{buffer}\n"));
                assert!(file(&f, "effects").is_empty());
            }
        }
    }

    #[tokio::test]
    async fn w1_terminal_paste_reclaim_bound_buffer() {
        let f = Fixture::with_paste_watchdog(Duration::from_secs(2));
        let old = writer(&f).await;
        stage(&f, &old, DRAFT).await;
        set(&f, "paste-mode", "timeout-before");
        assert_eq!(
            paste_reply(&f.engine, commit(&old, DRAFT.len())).await,
            PasteResult::Unknown {}
        );
        witness(&f, "server-parked").await;
        let buffer = text(&f, "last-load");
        let unrelated = format!("rs-paste-{}", Uuid::new_v4().simple());
        set(&f, &unrelated, "retained");
        let (mut survivor, _process) = f.second_pane(false);
        let stop = || Request::Stop {
            runtime_id: old.terminal.target.runtime_id,
            expected_pane_id: old.terminal.target.pane.clone(),
        };

        set(&f, "cleanup-blocked", "1");
        assert!(f.engine.handle(stop(), None).await.is_err());
        assert!(text(&f, "marker").contains("~q~"));
        assert_eq!(file(&f, &buffer), DRAFT);
        assert!(file(&f, "removed-0").is_empty());
        assert_held(&f.engine, &old).await;
        set(&f, "allow-effect", "1");
        witness(&f, "late-result").await;
        assert!(text(&f, "late-result").starts_with("rs-paste-rejected-"));
        assert!(file(&f, "effects").is_empty());
        assert_eq!(file(&f, &buffer), DRAFT);

        let (engine, terminal) = f.restart();
        assert_held(
            &engine,
            &PasteBinding {
                terminal,
                ..old.clone()
            },
        )
        .await;
        survivor.agent_epoch = engine.agent_epoch;
        let Response::TerminalControl { token, epoch } = engine
            .handle(
                Request::AcquireTerminal {
                    terminal: survivor.clone(),
                },
                None,
            )
            .await
            .unwrap()
        else {
            panic!("Expected the survivor to acquire control");
        };
        let survivor = PasteBinding {
            terminal: survivor,
            token,
            epoch,
            ..old.clone()
        };
        assert!(matches!(
            engine.handle(ordinary(&survivor, 0), None).await.unwrap(),
            Response::Accepted {}
        ));
        set(&f, "cleanup-blocked", "");
        assert!(matches!(
            engine.handle(stop(), None).await.unwrap(),
            Response::Stopped { .. }
        ));
        assert_eq!(file(&f, "removed-0"), b"1");
        assert!(!f.temp.path().join(&buffer).exists());
        assert_eq!(text(&f, "deletions"), format!("{buffer}\n"));
        assert_eq!(file(&f, &unrelated), b"retained");
        assert!(matches!(
            engine.handle(ordinary(&survivor, 1), None).await.unwrap(),
            Response::Accepted {}
        ));
        assert!(engine.handle(ordinary(&old, 0), None).await.is_err());
        assert_eq!(file(&f, "pane1-bytes"), b"KK");
        assert!(file(&f, "pane0-bytes").is_empty());
        assert!(file(&f, "effects").is_empty());
        assert_eq!(file(&f, "submissions"), b"1");
    }

    #[tokio::test]
    async fn w1_terminal_paste_requires_canonical_targets() {
        let f = Fixture::paste();
        for pane in ["%+0", "%00", "%4294967296", "-t", "%0 ; list-panes"] {
            let mut terminal = f.terminal.clone();
            terminal.target.pane = pane.into();
            assert!(
                f.engine
                    .handle(Request::AcquireTerminal { terminal }, None)
                    .await
                    .is_err()
            );
        }
        for (session, window) in [("$00", "@0"), ("$0", "@+0")] {
            let mut terminal = f.terminal.clone();
            terminal.target.session = session.into();
            terminal.target.window = window.into();
            assert!(
                f.engine
                    .handle(Request::AcquireTerminal { terminal }, None)
                    .await
                    .is_err()
            );
        }
        assert!(file(&f, "geometry-targets").is_empty());
        assert!(pane_identity(&["$0", "@0", "%0", "1"]).is_some());
        assert!(pane_identity(&["$0", "@00", "%0", "1"]).is_none());
        writer(&f).await;
        assert!(!file(&f, "geometry-targets").is_empty());
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
