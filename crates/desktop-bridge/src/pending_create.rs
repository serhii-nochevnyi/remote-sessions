use super::DesktopBridge;
use anyhow::{Context, Result, bail, ensure};
use remote_sessions_host::{
    CreateDeliveryError,
    model::{
        CreateState, PaneLiveness, ProductIssue, ProductRequest, ProductResponse, ProviderProfile,
        Runtime, RuntimeOrigin, tmux_name,
    },
    product_request, submit_product_create, validate_directory,
};
use serde::Serialize;
use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};
use uuid::Uuid;

const VERSION: &str = "remote-sessions-create-1";
const PREFIX: &str = "operation-";
const LIMIT: u64 = 512;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum CreateDisposition {
    Pending {},
    Failed {
        issue: ProductIssue,
    },
    Unknown {
        issue: Option<ProductIssue>,
    },
    Launched {
        runtime: Runtime,
        current: PaneLiveness,
        readiness: ProviderReadiness,
    },
    Exited {
        runtime: Runtime,
        current: PaneLiveness,
        setup_issue: Option<ProductIssue>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderReadiness {
    Unobserved,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LocalCreate {
    pub reference: Uuid,
    pub workspace_id: Uuid,
    pub profile: ProviderProfile,
    pub presented: bool,
    pub disposition: CreateDisposition,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum CreateSubmission {
    Recorded { operation: LocalCreate },
    Blocked { issue: ProductIssue },
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct LocalCreateStatus {
    pub operations: Vec<LocalCreate>,
    pub issue: Option<ProductIssue>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Submission {
    Fresh,
    Sent,
    Rejected(ProductIssue),
}

#[derive(Debug, Clone)]
struct Pending {
    reference: Uuid,
    key: Uuid,
    workspace_id: Uuid,
    profile: ProviderProfile,
    previous: Option<Uuid>,
    presented: bool,
    submission: Submission,
}

impl Pending {
    fn decode(text: &str, reference: Uuid) -> Result<Self> {
        let mut lines = text
            .strip_suffix('\n')
            .context("unterminated pending record")?
            .split('\n');
        ensure!(lines.next() == Some(VERSION), "unsupported pending record");
        let mut next = || lines.next().context("incomplete pending record");
        let key = parse_id(next()?)?;
        let workspace_id = parse_id(next()?)?;
        let profile = match next()? {
            "claude_code" => ProviderProfile::ClaudeCode,
            "codex" => ProviderProfile::Codex,
            _ => bail!("invalid pending profile"),
        };
        let previous = match next()? {
            "-" => None,
            id => Some(parse_id(id)?),
        };
        let presented = match next()? {
            "shown" => true,
            "unshown" => false,
            _ => bail!("invalid pending presentation"),
        };
        let submission = match next()? {
            "fresh" => Submission::Fresh,
            "sent" => Submission::Sent,
            "rejected" => Submission::Rejected(
                ProductIssue::decode(next()?).context("invalid rejection in pending record")?,
            ),
            _ => bail!("invalid submission in pending record"),
        };
        ensure!(
            lines.next().is_none() && previous != Some(reference) && key != reference,
            "invalid pending record"
        );
        Ok(Self {
            reference,
            key,
            workspace_id,
            profile,
            previous,
            presented,
            submission,
        })
    }

    fn encode(&self) -> String {
        let profile = match self.profile {
            ProviderProfile::ClaudeCode => "claude_code",
            ProviderProfile::Codex => "codex",
        };
        let previous = self
            .previous
            .map_or_else(|| "-".into(), |id| id.to_string());
        let presented = if self.presented { "shown" } else { "unshown" };
        let submission = match self.submission {
            Submission::Fresh => "fresh".into(),
            Submission::Sent => "sent".into(),
            Submission::Rejected(issue) => format!("rejected\n{}", issue.encode()),
        };
        format!(
            "{VERSION}\n{}\n{}\n{profile}\n{previous}\n{presented}\n{submission}\n",
            self.key, self.workspace_id
        )
    }

    fn display(&self, disposition: CreateDisposition) -> LocalCreate {
        LocalCreate {
            reference: self.reference,
            workspace_id: self.workspace_id,
            profile: self.profile,
            presented: self.presented,
            disposition,
        }
    }

    async fn state(&self, directory: &Path) -> CreateState {
        match self.submission {
            Submission::Fresh => CreateState::Pending {
                reservation: Uuid::nil(),
            },
            Submission::Rejected(issue) => CreateState::Failed { issue },
            Submission::Sent => {
                product_request(directory, &ProductRequest::CreateStatus { key: self.key })
                    .await
                    .map_or(CreateState::Unknown { issue: None }, response_state)
            }
        }
    }

    fn present(&self, state: CreateState) -> LocalCreate {
        self.display(match state {
            CreateState::Pending { .. } => CreateDisposition::Pending {},
            CreateState::Failed { issue } => CreateDisposition::Failed { issue },
            CreateState::Unknown { issue } => CreateDisposition::Unknown { issue },
            CreateState::Absent {} => CreateDisposition::Unknown { issue: None },
            CreateState::Confirmed {
                outcome,
                current,
                setup_issue,
            } if outcome.runtime.workspace_id == Some(self.workspace_id)
                && outcome.runtime.origin == RuntimeOrigin::Managed
                && outcome.runtime.operation_id.is_some_and(|id| !id.is_nil())
                && !outcome.runtime.runtime_id.is_nil()
                && !outcome.runtime.session_id.is_nil()
                && outcome.runtime.tmux_name
                    == tmux_name(outcome.runtime.session_id, outcome.runtime.runtime_id) =>
            {
                let mut runtime = outcome.runtime;
                if current == PaneLiveness::Exited
                    || (current == PaneLiveness::Unavailable && runtime.exited)
                {
                    runtime.exited = true;
                    CreateDisposition::Exited {
                        runtime,
                        current,
                        setup_issue,
                    }
                } else {
                    runtime.exited = false;
                    CreateDisposition::Launched {
                        runtime,
                        current,
                        readiness: ProviderReadiness::Unobserved,
                    }
                }
            }
            CreateState::Confirmed { .. } => CreateDisposition::Unknown { issue: None },
        })
    }
}

fn response_state(response: ProductResponse) -> CreateState {
    match response {
        ProductResponse::Create { state } => state,
        ProductResponse::Blocked { issue } => CreateState::Unknown { issue: Some(issue) },
        _ => CreateState::Unknown { issue: None },
    }
}

fn parse_id(text: &str) -> Result<Uuid> {
    let id = Uuid::try_parse(text)?;
    ensure!(!id.is_nil(), "missing identifier");
    Ok(id)
}

fn private_file(file: &File, directory: &Path) -> Result<()> {
    let meta = file.metadata()?;
    ensure!(
        meta.is_file()
            && meta.uid() == directory.metadata()?.uid()
            && meta.mode() & 0o7777 == 0o600
            && meta.nlink() == 1,
        "unsafe pending file"
    );
    Ok(())
}

fn options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.mode(0o600);
    options
}

struct Store {
    path: PathBuf,
    _lock: File,
}

impl Store {
    fn open(directory: &Path) -> Result<Self> {
        validate_directory(directory)?;
        let path = directory.join("pending-create");
        let _created = match DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => true,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => false,
            Err(error) => return Err(error.into()),
        };
        let meta = fs::symlink_metadata(&path)?;
        ensure!(
            meta.is_dir()
                && meta.mode() & 0o7777 == 0o700
                && meta.uid() == directory.metadata()?.uid(),
            "unsafe pending directory"
        );
        let sync_parent = || File::open(directory)?.sync_all();
        #[cfg(test)]
        let sync_parent = || tests::observe_parent_sync(_created, sync_parent);
        sync_parent()?;
        let lock_path = path.join("lock");
        let lock = match options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => open_record(&lock_path)?,
            Err(error) => return Err(error.into()),
        };
        private_file(&lock, &path)?;
        lock.try_lock().context("native creation store is busy")?;
        Ok(Self { path, _lock: lock })
    }

    fn record_path(&self, reference: Uuid) -> PathBuf {
        self.path.join(format!("{PREFIX}{reference}"))
    }

    fn read(&self, reference: Uuid) -> Result<Pending> {
        let file = open_record(&self.record_path(reference))?;
        private_file(&file, &self.path)?;
        let mut text = String::new();
        file.take(LIMIT + 1).read_to_string(&mut text)?;
        ensure!(text.len() as u64 <= LIMIT, "pending record exceeds limit");
        Pending::decode(&text, reference)
    }

    fn list(&self) -> Result<(Vec<Pending>, bool)> {
        let mut records = Vec::new();
        let mut unreadable = false;
        for entry in fs::read_dir(&self.path)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(suffix) = name.to_str().and_then(|name| name.strip_prefix(PREFIX)) else {
                continue;
            };
            match parse_id(suffix).and_then(|reference| self.read(reference)) {
                Ok(record) => records.push(record),
                Err(_) => unreadable = true,
            }
        }
        records.sort_unstable_by_key(|record| record.reference);
        Ok((records, unreadable))
    }

    fn write(&self, pending: &Pending) -> Result<()> {
        let destination = self.record_path(pending.reference);
        // @invariant: Preserve unreadable records as evidence of a possible prior submission.
        match fs::symlink_metadata(&destination) {
            Ok(_) => {
                let saved = self.read(pending.reference)?;
                ensure!(saved.key == pending.key, "pending reference collision");
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let temp = self.path.join(format!(".pending-{}", Uuid::new_v4()));
        let mut file = options().write(true).create_new(true).open(&temp)?;
        let result = (|| -> Result<()> {
            private_file(&file, &self.path)?;
            file.write_all(pending.encode().as_bytes())?;
            file.sync_all()?;
            fs::rename(&temp, destination)?;
            File::open(&self.path)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }
}

fn open_record(path: &Path) -> Result<File> {
    let disk = fs::symlink_metadata(path)?;
    ensure!(disk.is_file(), "unsafe pending record type");
    let file = options().read(true).open(path)?;
    let opened = file.metadata()?;
    ensure!(
        (disk.dev(), disk.ino()) == (opened.dev(), opened.ino()),
        "pending record changed"
    );
    Ok(file)
}

impl DesktopBridge {
    pub async fn create_session(
        &self,
        workspace_id: &str,
        profile: ProviderProfile,
        separate_from: Option<&str>,
        continue_reference: Option<&str>,
    ) -> CreateSubmission {
        let blocked = |issue| CreateSubmission::Blocked { issue };
        let Ok(workspace_id) = parse_id(workspace_id) else {
            return blocked(ProductIssue::InvalidRequest);
        };
        let Ok(previous) = separate_from.map(parse_id).transpose() else {
            return blocked(ProductIssue::InvalidRequest);
        };
        let Ok(continuing) = continue_reference.map(parse_id).transpose() else {
            return blocked(ProductIssue::InvalidRequest);
        };
        if previous.is_some() && continuing.is_some() {
            return blocked(ProductIssue::InvalidRequest);
        }
        let Ok(directory) = self.directory() else {
            return blocked(ProductIssue::PendingUnavailable);
        };
        let _submit = self.creates.lock().await;

        if let Some(previous) = previous {
            let Ok(prior) = Store::open(directory).and_then(|store| store.read(previous)) else {
                return blocked(ProductIssue::PendingUnavailable);
            };
            if (prior.workspace_id, prior.profile) != (workspace_id, profile)
                || matches!(prior.state(directory).await, CreateState::Pending { .. })
            {
                return blocked(ProductIssue::InvalidRequest);
            }
        }

        let saved = (|| -> Result<(Pending, bool)> {
            let store = Store::open(directory)?;
            let (records, unreadable) = store.list()?;
            ensure!(!unreadable, "pending evidence needs attention");
            let mut matching = records.into_iter().filter(|record| {
                (record.workspace_id, record.profile, record.previous)
                    == (workspace_id, profile, previous)
            });
            let mut operation = if let Some(reference) = continuing {
                let existing = store.read(reference)?;
                ensure!(
                    (existing.workspace_id, existing.profile) == (workspace_id, profile),
                    "pending binding changed"
                );
                existing
            } else {
                match matching.next() {
                    Some(existing) => existing,
                    None => {
                        let new = Pending {
                            reference: Uuid::new_v4(),
                            key: Uuid::new_v4(),
                            workspace_id,
                            profile,
                            previous,
                            presented: false,
                            submission: Submission::Fresh,
                        };
                        store.write(&new)?;
                        new
                    }
                }
            };
            if continuing.is_none() {
                ensure!(matching.next().is_none(), "duplicate pending binding");
            }
            let first_send = operation.submission == Submission::Fresh;
            if first_send {
                operation.submission = Submission::Sent;
                store.write(&operation)?;
            }
            Ok((operation, first_send))
        })();
        let Ok((mut operation, first_send)) = saved else {
            return blocked(ProductIssue::PendingUnavailable);
        };
        let reservation = if first_send {
            None
        } else {
            let state = operation.state(directory).await;
            match state {
                CreateState::Pending { reservation } if !reservation.is_nil() => Some(reservation),
                state => {
                    return CreateSubmission::Recorded {
                        operation: operation.present(state),
                    };
                }
            }
        };
        let state = match submit_product_create(
            directory,
            &ProductRequest::CreateManaged {
                key: operation.key,
                workspace_id,
                profile,
                reservation,
            },
        )
        .await
        {
            Ok(response) => response_state(response),
            Err(CreateDeliveryError::BeforeSend) if first_send => {
                // @invariant: Reset to fresh only when the first attempt could not write a request.
                operation.submission = Submission::Fresh;
                let saved = Store::open(directory).and_then(|store| store.write(&operation));
                let disposition = if saved.is_ok() {
                    CreateDisposition::Pending {}
                } else {
                    CreateDisposition::Unknown {
                        issue: Some(ProductIssue::PendingUnavailable),
                    }
                };
                return CreateSubmission::Recorded {
                    operation: operation.display(disposition),
                };
            }
            Err(_) => CreateState::Unknown { issue: None },
        };
        if let CreateState::Failed { issue } = &state {
            operation.submission = Submission::Rejected(*issue);
            let _ = Store::open(directory).and_then(|store| store.write(&operation));
        }
        CreateSubmission::Recorded {
            operation: operation.present(state),
        }
    }

    pub async fn create_status(&self, reference: Option<&str>) -> LocalCreateStatus {
        let result = (|| {
            let directory = self.directory()?;
            let store = Store::open(directory)?;
            let (records, unreadable) = match reference {
                Some(reference) => (vec![store.read(parse_id(reference)?)?], false),
                None => store.list()?,
            };
            Ok((directory, records, unreadable))
        })();
        let Ok((directory, records, unreadable)): Result<_> = result else {
            return LocalCreateStatus {
                operations: Vec::new(),
                issue: Some(ProductIssue::PendingUnavailable),
            };
        };
        let mut operations = Vec::with_capacity(records.len());
        for record in records {
            let state = record.state(directory).await;
            operations.push(record.present(state));
        }
        LocalCreateStatus {
            operations,
            issue: unreadable.then_some(ProductIssue::PendingUnavailable),
        }
    }

    pub async fn acknowledge_create(&self, reference: &str) -> LocalCreateStatus {
        let mut status = self.create_status(Some(reference)).await;
        let Some(operation) = status.operations.first_mut() else {
            return status;
        };
        if operation.presented
            || !matches!(
                operation.disposition,
                CreateDisposition::Launched {
                    current: PaneLiveness::Live,
                    ..
                } | CreateDisposition::Exited {
                    current: PaneLiveness::Exited,
                    ..
                }
            )
        {
            return status;
        }
        let result = (|| -> Result<()> {
            let store = Store::open(self.directory()?)?;
            let mut record = store.read(operation.reference)?;
            ensure!(
                record.submission == Submission::Sent,
                "launch record changed"
            );
            record.presented = true;
            store.write(&record)
        })();
        if result.is_ok() {
            operation.presented = true;
        } else {
            status.issue = Some(ProductIssue::PendingUnavailable);
        }
        status
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remote_sessions_host::{
        decode_local_fixture, encode_local_observation, model::LaunchOutcome,
    };
    #[cfg(debug_assertions)]
    use std::os::unix::fs::PermissionsExt;
    use std::{
        fs::TryLockError,
        os::unix::net::UnixListener as RawListener,
        sync::{Arc, mpsc as thread_channel},
        time::Duration,
    };
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::{UnixListener, UnixStream},
        sync::mpsc,
        task::JoinHandle,
    };

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let root = Path::new("/tmp")
                .canonicalize()
                .unwrap()
                .join(format!("rs-{}", Uuid::new_v4().simple()));
            DirBuilder::new().mode(0o700).create(&root).unwrap();
            Self(root)
        }

        fn directory(&self, name: &str) -> PathBuf {
            let directory = self.0.join(name);
            DirBuilder::new().mode(0o700).create(&directory).unwrap();
            directory
        }

        fn bridge(&self) -> Arc<DesktopBridge> {
            Arc::new(DesktopBridge::new(
                Some(self.0.clone()),
                "/unused/tmux".into(),
            ))
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    async fn wait_store_lock_release(directory: &Path) -> Result<()> {
        let path = directory.join("pending-create");
        let lock = open_record(&path.join("lock")).context("open pending creation fixture lock")?;
        private_file(&lock, &path).context("validate pending creation fixture lock")?;
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match lock.try_lock() {
                    Ok(()) => {
                        return lock
                            .unlock()
                            .context("unlock pending creation fixture lock");
                    }
                    Err(TryLockError::WouldBlock) => {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    Err(TryLockError::Error(error)) => {
                        return Err(error).context("acquire pending creation fixture lock");
                    }
                }
            }
        })
        .await
        .context("pending creation fixture lock remained owned")?
    }

    async fn frame(stream: &mut UnixStream) -> String {
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).await.unwrap();
        line
    }

    fn recorded(submission: CreateSubmission) -> LocalCreate {
        let CreateSubmission::Recorded { operation } = submission else {
            panic!("expected recorded operation")
        };
        operation
    }

    const FIXTURE_TIMEOUT: Duration = Duration::from_secs(5);
    const PARK_TIMEOUT: Duration = Duration::from_secs(30);
    const OPENED_NEW: SyncObservation = SyncObservation::Entered { created: true };
    const OPENED_EXISTING: SyncObservation = SyncObservation::Entered { created: false };
    const PARENT_SYNC_DENIED: Result<(), io::ErrorKind> = Err(io::ErrorKind::PermissionDenied);
    const UNAVAILABLE: CreateSubmission = CreateSubmission::Blocked {
        issue: ProductIssue::PendingUnavailable,
    };

    async fn wait_for<T>(future: impl Future<Output = T>) -> T {
        tokio::time::timeout(FIXTURE_TIMEOUT, future)
            .await
            .expect("pending create fixture timed out")
    }

    #[derive(Debug, PartialEq, Eq)]
    enum SyncObservation {
        Entered { created: bool },
        Finished(Result<(), io::ErrorKind>),
    }

    struct ParentSyncGate {
        observations: mpsc::UnboundedSender<SyncObservation>,
        release: thread_channel::Receiver<Result<(), io::ErrorKind>>,
    }

    tokio::task_local! {
        static PARENT_SYNC: ParentSyncGate;
    }

    pub(super) fn observe_parent_sync(
        created: bool,
        sync: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<()> {
        let released = PARENT_SYNC
            .try_with(|gate| {
                gate.observations
                    .send(SyncObservation::Entered { created })
                    .unwrap();
                tokio::task::block_in_place(|| {
                    gate.release
                        .recv_timeout(PARK_TIMEOUT)
                        .expect("parent sync was not released")
                })
                .map_err(io::Error::from)
            })
            .unwrap_or(Ok(()));
        let result = released.and_then(|()| sync());
        let _ = PARENT_SYNC.try_with(|gate| {
            gate.observations
                .send(SyncObservation::Finished(
                    result.as_ref().map(|_| ()).map_err(io::Error::kind),
                ))
                .unwrap();
        });
        result
    }

    struct GatedCreate {
        task: JoinHandle<CreateSubmission>,
        observations: mpsc::UnboundedReceiver<SyncObservation>,
        release: thread_channel::Sender<Result<(), io::ErrorKind>>,
    }

    impl GatedCreate {
        fn start(bridge: Arc<DesktopBridge>, workspace: Uuid, continuing: Option<Uuid>) -> Self {
            let (observed, observations) = mpsc::unbounded_channel();
            let (release, resume) = thread_channel::channel();
            let gate = ParentSyncGate {
                observations: observed,
                release: resume,
            };
            let task = tokio::spawn(PARENT_SYNC.scope(gate, async move {
                let continuing = continuing.map(|reference| reference.to_string());
                bridge
                    .create_session(
                        &workspace.to_string(),
                        ProviderProfile::Codex,
                        None,
                        continuing.as_deref(),
                    )
                    .await
            }));
            Self {
                task,
                observations,
                release,
            }
        }

        async fn expect_sync(&mut self, observation: SyncObservation) {
            assert_eq!(wait_for(self.observations.recv()).await, Some(observation));
        }

        async fn release_sync(&mut self, result: Result<(), io::ErrorKind>) {
            self.release.send(result).unwrap();
            self.expect_sync(SyncObservation::Finished(result)).await;
        }

        async fn finish(self) -> CreateSubmission {
            wait_for(self.task).await.unwrap()
        }
    }

    fn pending_paths(directory: &Path) -> Vec<PathBuf> {
        fs::read_dir(directory.join("pending-create"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(PREFIX)
            })
            .collect()
    }

    fn only_pending(directory: &Path) -> (Pending, String) {
        let paths = pending_paths(directory);
        let [path] = paths.as_slice() else {
            panic!("expected one pending record")
        };
        let name = path.file_name().unwrap().to_str().unwrap();
        let reference = parse_id(name.strip_prefix(PREFIX).unwrap()).unwrap();
        let bytes = fs::read_to_string(path).unwrap();
        (Pending::decode(&bytes, reference).unwrap(), bytes)
    }

    #[derive(serde::Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct FixtureEnvelope<T> {
        product: T,
    }

    struct CreatePeer {
        socket: RawListener,
        listener: UnixListener,
        runtime: Runtime,
        creates: Vec<Uuid>,
    }

    impl CreatePeer {
        fn bind(directory: &Path, workspace: Uuid) -> Self {
            let socket = RawListener::bind(directory.join("agent.sock")).unwrap();
            socket.set_nonblocking(true).unwrap();
            let listener = UnixListener::from_std(socket.try_clone().unwrap()).unwrap();
            let session_id = Uuid::new_v4();
            let runtime_id = Uuid::new_v4();
            let runtime = Runtime {
                session_id,
                runtime_id,
                tmux_name: tmux_name(session_id, runtime_id),
                pane_id: "%3".into(),
                command: "codex".into(),
                pane_pid: Some(501),
                exited: false,
                width: 100,
                height: 30,
                origin: RuntimeOrigin::Managed,
                workspace_id: Some(workspace),
                operation_id: Some(Uuid::new_v4()),
            };
            Self {
                socket,
                listener,
                runtime,
                creates: Vec::new(),
            }
        }

        fn assert_quiet(&self) {
            assert_eq!(
                self.socket.accept().unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
        }

        async fn accept(&self) -> UnixStream {
            wait_for(self.listener.accept()).await.unwrap().0
        }

        async fn request(stream: &mut UnixStream) -> ProductRequest {
            let line = wait_for(frame(stream)).await;
            decode_local_fixture::<FixtureEnvelope<ProductRequest>>(line.as_bytes())
                .unwrap()
                .product
        }

        async fn confirm(&self, stream: &mut UnixStream) {
            let response = FixtureEnvelope {
                product: ProductResponse::Create {
                    state: CreateState::Confirmed {
                        outcome: LaunchOutcome {
                            runtime: self.runtime.clone(),
                            setup_issue: None,
                        },
                        current: PaneLiveness::Live,
                        setup_issue: None,
                    },
                },
            };
            let mut line = encode_local_observation(&response).unwrap();
            line.push('\n');
            stream.write_all(line.as_bytes()).await.unwrap();
        }

        async fn confirm_create(&mut self, directory: &Path) -> Pending {
            let mut stream = self.accept().await;
            let (saved, _) = only_pending(directory);
            assert_eq!(saved.submission, Submission::Sent);
            assert!(!saved.presented);
            assert_eq!(saved.previous, None);
            let ProductRequest::CreateManaged {
                key,
                workspace_id,
                profile,
                reservation,
            } = Self::request(&mut stream).await
            else {
                panic!("expected a create request")
            };
            assert_eq!(
                (key, workspace_id, profile, reservation),
                (saved.key, saved.workspace_id, saved.profile, None)
            );
            assert_eq!(Some(workspace_id), self.runtime.workspace_id);
            assert_eq!(profile, ProviderProfile::Codex);
            self.creates.push(key);
            self.confirm(&mut stream).await;
            saved
        }

        async fn confirm_status(&self, expected_key: Uuid) {
            let mut stream = self.accept().await;
            let ProductRequest::CreateStatus { key } = Self::request(&mut stream).await else {
                panic!("expected a status request")
            };
            assert_eq!(key, expected_key);
            self.confirm(&mut stream).await;
        }

        fn assert_confirmed(&self, operation: &LocalCreate, saved: &Pending) {
            assert_eq!(operation.reference, saved.reference);
            assert_eq!(operation.workspace_id, saved.workspace_id);
            assert_eq!(operation.profile, saved.profile);
            assert!(!operation.presented);
            assert_eq!(
                operation.disposition,
                CreateDisposition::Launched {
                    runtime: self.runtime.clone(),
                    current: PaneLiveness::Live,
                    readiness: ProviderReadiness::Unobserved,
                }
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pending_create_overlapping_first_use_requires_parent_sync() {
        let fixture = Fixture::new();
        let directory = &fixture.0;
        let workspace = Uuid::new_v4();
        let mut peer = CreatePeer::bind(directory, workspace);
        let b = fixture.bridge();
        let mut first = GatedCreate::start(fixture.bridge(), workspace, None);
        first.expect_sync(OPENED_NEW).await;
        assert!(pending_paths(directory).is_empty());
        peer.assert_quiet();

        let mut failed = GatedCreate::start(Arc::clone(&b), workspace, None);
        failed.expect_sync(OPENED_EXISTING).await;
        assert!(pending_paths(directory).is_empty());
        peer.assert_quiet();
        failed.release_sync(PARENT_SYNC_DENIED).await;
        assert_eq!(failed.finish().await, UNAVAILABLE);
        assert!(pending_paths(directory).is_empty());
        peer.assert_quiet();

        let mut second = GatedCreate::start(Arc::clone(&b), workspace, None);
        second.expect_sync(OPENED_EXISTING).await;
        assert!(pending_paths(directory).is_empty());
        peer.assert_quiet();
        second.release_sync(Ok(())).await;
        let saved = peer.confirm_create(directory).await;
        let operation = recorded(second.finish().await);
        peer.assert_confirmed(&operation, &saved);

        let reference = operation.reference.to_string();
        let (status, ()) = tokio::join!(
            b.create_status(Some(&reference)),
            peer.confirm_status(saved.key)
        );
        assert_eq!(
            status,
            LocalCreateStatus {
                operations: vec![operation.clone()],
                issue: None
            }
        );
        first.release_sync(Ok(())).await;
        peer.confirm_status(saved.key).await;
        assert_eq!(recorded(first.finish().await), operation);
        assert_eq!(only_pending(directory).0.key, saved.key);
        assert_eq!(peer.creates, [saved.key]);
        peer.assert_quiet();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pending_create_interrupted_init_preserves_key_and_blocks_unsynced_submit() {
        let fixture = Fixture::new();
        let directory = &fixture.0;
        let workspace = Uuid::new_v4();
        let peer = CreatePeer::bind(directory, workspace);
        let mut first = GatedCreate::start(fixture.bridge(), workspace, None);
        first.expect_sync(OPENED_NEW).await;
        first.release_sync(Err(io::ErrorKind::Interrupted)).await;
        assert_eq!(first.finish().await, UNAVAILABLE);
        assert!(pending_paths(directory).is_empty());
        peer.assert_quiet();

        let bridge = fixture.bridge();
        let mut failed = GatedCreate::start(Arc::clone(&bridge), workspace, None);
        failed.expect_sync(OPENED_EXISTING).await;
        failed.release_sync(PARENT_SYNC_DENIED).await;
        assert_eq!(failed.finish().await, UNAVAILABLE);
        assert!(pending_paths(directory).is_empty());
        peer.assert_quiet();

        drop(peer);
        fs::remove_file(directory.join("agent.sock")).unwrap();
        let pending = recorded(
            bridge
                .create_session(&workspace.to_string(), ProviderProfile::Codex, None, None)
                .await,
        );
        assert_eq!(pending.disposition, CreateDisposition::Pending {});
        let (original, bytes) = only_pending(directory);
        assert_eq!(original.reference, pending.reference);
        assert_eq!(original.submission, Submission::Fresh);

        let mut peer = CreatePeer::bind(directory, workspace);
        let restarted = fixture.bridge();
        let mut failed = GatedCreate::start(Arc::clone(&restarted), workspace, None);
        failed.expect_sync(OPENED_EXISTING).await;
        peer.assert_quiet();
        failed.release_sync(PARENT_SYNC_DENIED).await;
        assert_eq!(failed.finish().await, UNAVAILABLE);
        assert_eq!(only_pending(directory).1, bytes);
        peer.assert_quiet();

        let mut retry = GatedCreate::start(restarted, workspace, Some(original.reference));
        retry.expect_sync(OPENED_EXISTING).await;
        assert_eq!(only_pending(directory).1, bytes);
        peer.assert_quiet();
        retry.release_sync(Ok(())).await;
        let sent = peer.confirm_create(directory).await;
        assert_eq!(
            (sent.reference, sent.key),
            (original.reference, original.key)
        );
        peer.assert_confirmed(&recorded(retry.finish().await), &sent);
        assert_eq!(peer.creates, [original.key]);
        peer.assert_quiet();
    }

    #[tokio::test]
    async fn pending_create_failed_store_and_cancel_send_no_create_request() {
        let fixture = Fixture::new();
        fs::write(fixture.0.join("pending-create"), "preserve this").unwrap();
        let listener = UnixListener::bind(fixture.0.join("agent.sock")).unwrap();
        let bridge = DesktopBridge::new(Some(fixture.0.clone()), "/unused/tmux".into());
        assert_eq!(
            bridge.pick_workspace(None).await.unwrap(),
            crate::WorkspaceSelection::Cancelled {}
        );
        assert_eq!(
            bridge
                .create_session(
                    &Uuid::new_v4().to_string(),
                    ProviderProfile::Codex,
                    None,
                    None
                )
                .await,
            CreateSubmission::Blocked {
                issue: ProductIssue::PendingUnavailable
            }
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
        assert_eq!(
            fs::read_to_string(fixture.0.join("pending-create")).unwrap(),
            "preserve this"
        );
    }

    #[tokio::test]
    async fn pending_create_separate_unknowns_keep_references_and_unreadable_records() {
        let fixture = Fixture::new();
        let directory = &fixture.0;
        let listener = UnixListener::bind(directory.join("agent.sock")).unwrap();
        let workspace = Uuid::new_v4().to_string();
        let server = async {
            let (mut initial, _) = listener.accept().await.unwrap();
            let create_one = frame(&mut initial).await;
            assert!(create_one.contains("\"op\":\"create_managed\""));
            wait_store_lock_release(directory).await.unwrap();
            let one = Store::open(directory).unwrap().list().unwrap().0.remove(0);
            assert_eq!(one.submission, Submission::Sent);
            assert!(create_one.contains(&one.key.to_string()));
            drop(initial);

            let (mut status, _) = listener.accept().await.unwrap();
            assert!(frame(&mut status).await.contains(&one.key.to_string()));
            status
                .write_all(
                    b"{\"product\":{\"result\":\"create\",\"state\":{\"state\":\"absent\"}}}\n",
                )
                .await
                .unwrap();

            let (mut second, _) = listener.accept().await.unwrap();
            let create_two = frame(&mut second).await;
            assert!(create_two.contains("\"op\":\"create_managed\""));
            wait_store_lock_release(directory).await.unwrap();
            let records = Store::open(directory).unwrap().list().unwrap().0;
            let two = records
                .iter()
                .find(|record| record.previous == Some(one.reference))
                .unwrap();
            assert!(
                create_two.contains(&two.key.to_string())
                    && !create_two.contains(&one.key.to_string())
            );
            drop(second);

            for record in [&one, two] {
                let (mut query, _) = listener.accept().await.unwrap();
                let line = frame(&mut query).await;
                assert!(
                    line.contains("\"op\":\"create_status\"")
                        && line.contains(&record.key.to_string())
                );
                query.write_all(b"{\"product\":{\"result\":\"create\",\"state\":{\"state\":\"unknown\",\"issue\":null}}}\n").await.unwrap();
            }
        };
        let client = async {
            let bridge = DesktopBridge::new(Some(directory.clone()), "/unused/tmux".into());
            let one = recorded(
                bridge
                    .create_session(&workspace, ProviderProfile::Codex, None, None)
                    .await,
            );
            assert_eq!(one.disposition, CreateDisposition::Unknown { issue: None });
            drop(bridge);
            let bridge = DesktopBridge::new(Some(directory.clone()), "/unused/tmux".into());
            let two = recorded(
                bridge
                    .create_session(
                        &workspace,
                        ProviderProfile::Codex,
                        Some(&one.reference.to_string()),
                        None,
                    )
                    .await,
            );
            assert_ne!(one.reference, two.reference);
            assert_eq!(two.disposition, CreateDisposition::Unknown { issue: None });
            for operation in [one, two] {
                let status = bridge
                    .create_status(Some(&operation.reference.to_string()))
                    .await;
                assert_eq!(status.operations, vec![operation]);
            }
        };
        tokio::time::timeout(Duration::from_secs(4), async {
            tokio::join!(server, client);
        })
        .await
        .unwrap();

        let path = {
            wait_store_lock_release(directory).await.unwrap();
            let store = Store::open(directory).unwrap();
            let records = store.list().unwrap().0;
            assert_eq!(records.len(), 2);
            store.record_path(records[0].reference)
        };
        fs::write(&path, "unsupported-version\n").unwrap();
        let bridge = DesktopBridge::new(Some(directory.clone()), "/unused/tmux".into());
        drop(listener);
        let status = bridge.create_status(None).await;
        assert_eq!(status.operations.len(), 1);
        assert_eq!(status.issue, Some(ProductIssue::PendingUnavailable));
        assert_eq!(fs::read_to_string(path).unwrap(), "unsupported-version\n");
    }

    #[tokio::test]
    async fn pending_create_no_connection_reuses_unsent_key() {
        let fixture = Fixture::new();
        let workspace = Uuid::new_v4().to_string();
        let directory = &fixture.0;
        let bridge = DesktopBridge::new(Some(directory.clone()), "/unused/tmux".into());
        let pending = recorded(
            bridge
                .create_session(&workspace, ProviderProfile::Codex, None, None)
                .await,
        );
        assert_eq!(pending.disposition, CreateDisposition::Pending {});
        wait_store_lock_release(directory).await.unwrap();
        let saved = Store::open(directory)
            .unwrap()
            .read(pending.reference)
            .unwrap();
        assert_eq!(saved.submission, Submission::Fresh);
        let listener = UnixListener::bind(directory.join("agent.sock")).unwrap();
        let server = async {
            let (mut stream, _) = listener.accept().await.unwrap();
            let sent = frame(&mut stream).await;
            assert!(
                sent.contains("\"op\":\"create_managed\"") && sent.contains(&saved.key.to_string())
            );
            stream.write_all(b"{\"product\":{\"result\":\"create\",\"state\":{\"state\":\"failed\",\"issue\":\"provider_missing\"}}}\n").await.unwrap();
        };
        let client = async {
            let result = recorded(
                bridge
                    .create_session(
                        &workspace,
                        ProviderProfile::Codex,
                        None,
                        Some(&pending.reference.to_string()),
                    )
                    .await,
            );
            assert_eq!(result.reference, pending.reference);
            assert_eq!(
                result.disposition,
                CreateDisposition::Failed {
                    issue: ProductIssue::ProviderMissing
                }
            );
            let restarted = DesktopBridge::new(Some(directory.clone()), "/unused/tmux".into());
            assert_eq!(restarted.create_status(None).await.operations, vec![result]);
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(server, client);
        })
        .await
        .unwrap();
    }

    #[cfg(debug_assertions)]
    #[tokio::test]
    async fn pending_create_committed_reply_dropped_then_bridge_restart_recovers_through_sqlite() {
        use crate::w1_terminal_tests::write_catalog_birth_fixture;
        use portable_pty::{PtySize, native_pty_system};

        let fixture = Fixture::new();
        let host = fixture.directory("host");
        let native = fixture.directory("native");
        let project = fixture.directory("Проєкт з пробілом");
        let tmux = host.join("tmux");
        let provider = host.join("codex");
        fs::write(&provider, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&provider, fs::Permissions::from_mode(0o700)).unwrap();
        let generation = Uuid::new_v4();
        fs::write(host.join("generation"), generation.to_string()).unwrap();
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: 30,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let tty = pty.master.tty_name().unwrap();
        write_catalog_birth_fixture(&host, &tty, "$1~@2~%3");
        let command = r##"#!/bin/sh
set -eu
cd "$(dirname "$3")"
token=$(cat generation)
pid=$(cat pane-pid)
birth_format='#{pid}|#{start_time}|#{@rs_create_generation}|#{session_id}|#{window_id}|#{pane_id}|#{pane_pid}|#{session_name}|#{window_index}|#{pane_index}|#{@rs_create_nonce}|#{pane_dead}|#{pane_tty}|#{@rs_create_initializer}|#{@rs_create_birth}'
case "$4" in
  display-message)
    if test "${6-}" = -t; then
      test "$#" = 8 && test "$5" = -p && test "$7" = '%3' && test "$8" = "$birth_format" || exit 1
      name=$(cat active)
      nonce=$(cat "$name.nonce")
      printf '52|101|%s|$1|@2|%%3|%s|%s|0|0|%s:$1:@2:%%3:%s|0|%s|%s|%s\n' \
        "$token" "$pid" "$name" "$nonce" "$pid" "$(cat tty)" \
        "$(cat "$name.initializer")" "$(cat "$name.birth")"
      exit 0
    fi
    printf '52|101|%s\n' "$token"
    if test "$#" -gt 6 && test -f names; then cat names; fi ;;
  if-shell)
    branch=$7
    case "$branch" in 'new-session '*) ;; *) exit 1 ;; esac
    case "$6" in *"$token"*) ;; *) exit 1 ;; esac
    printf 'marked\n' >> dispatched
    exec ./spawn-tmux "$@" ;;
  show-environment)
    name=${6#=}
    test "$#" = 14 && test "$7" = RS_CREATE_NONCE
    nonce=$(cat "$name.nonce")
    printf 'RS_CREATE_NONCE=%s\n52|101|%s|$1|@2|%%3|%s|%s|0|0|%s:$1:@2:%%3:%s|0|100|30\n' \
      "$nonce" "$token" "$pid" "$name" "$nonce" "$pid" ;;
  list-sessions) if test -f names; then cat names; fi ;;
  list-panes)
    printf '%s\t%%3\t%s\t0\t100\t30\tcodex' "$(cat active)" "$pid"
    case "$7" in
      *'#{pid}|#{start_time}|#{?#{m/r:^[0-9a-f-]*$,#{@rs_create_generation}},#{@rs_create_generation},invalid}|#{session_id}|#{window_id}'*)
        printf '\t52|101|%s|$1|@2' "$token" ;;
    esac
    printf '\n' ;;
  *) exit 1 ;;
esac
"##;
        fs::write(&tmux, command).unwrap();
        fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();
        let host_run = host.clone();
        let catalog = fixture.0.join("catalog");
        let server = tokio::spawn(async move {
            let service_path = host_run.clone();
            remote_sessions_host::serve_catalog_fixture(
                host_run,
                tmux,
                catalog,
                service_path.as_os_str(),
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !host.join("agent.sock").exists() {
                assert!(!server.is_finished());
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let listener = UnixListener::bind(native.join("agent.sock")).unwrap();
        let relay_host = host.clone();
        let relay_native = native.clone();
        let relay = tokio::spawn(async move {
            let mut dropped = false;
            let mut key = None;
            loop {
                let (mut browser, _) = listener.accept().await.unwrap();
                let input = frame(&mut browser).await;
                let mut agent = UnixStream::connect(relay_host.join("agent.sock"))
                    .await
                    .unwrap();
                agent.write_all(input.as_bytes()).await.unwrap();
                let response = frame(&mut agent).await;
                if input.contains("\"op\":\"create_managed\"") {
                    assert!(!dropped, "create was replayed");
                    wait_store_lock_release(&relay_native).await.unwrap();
                    let saved = Store::open(&relay_native)
                        .unwrap()
                        .list()
                        .unwrap()
                        .0
                        .remove(0);
                    assert!(input.contains(&saved.key.to_string()));
                    assert_eq!(saved.submission, Submission::Sent);
                    assert!(response.contains("\"state\":\"confirmed\""));
                    key = Some(saved.key);
                    dropped = true;
                } else {
                    if input.contains("\"op\":\"create_status\"") {
                        assert!(input.contains(&key.unwrap().to_string()));
                    }
                    browser.write_all(response.as_bytes()).await.unwrap();
                }
            }
        });

        let bridge = DesktopBridge::new(Some(native.clone()), "/unused/tmux".into());
        let crate::WorkspaceSelection::Selected { workspace } =
            bridge.pick_workspace(Some(project)).await.unwrap()
        else {
            panic!("registration did not traverse private IPC")
        };
        let workspace_id = workspace.workspace_id.to_string();
        let initial = recorded(
            bridge
                .create_session(&workspace_id, ProviderProfile::Codex, None, None)
                .await,
        );
        assert_eq!(
            initial.disposition,
            CreateDisposition::Unknown { issue: None }
        );
        drop(bridge);
        let restarted = DesktopBridge::new(Some(native), "/unused/tmux".into());
        let status = restarted.create_status(None).await;
        assert_eq!(status.issue, None);
        assert_eq!(status.operations.len(), 1);
        let recovered = &status.operations[0];
        assert_eq!(recovered.reference, initial.reference);
        let CreateDisposition::Launched {
            runtime,
            current: PaneLiveness::Live,
            ..
        } = &recovered.disposition
        else {
            panic!("expected the committed launch")
        };
        assert_eq!(runtime.pane_id, "%3");
        assert_eq!(runtime.pane_pid, Some(std::process::id()));
        let inventory = restarted.inventory().await.unwrap();
        assert_eq!(inventory.runtimes, vec![runtime.clone()]);
        let (one, two) = tokio::join!(
            restarted.create_session(&workspace_id, ProviderProfile::Codex, None, None),
            restarted.create_session(&workspace_id, ProviderProfile::Codex, None, None),
        );
        assert_eq!(&recorded(one), recovered);
        assert_eq!(&recorded(two), recovered);
        let presented = restarted
            .acknowledge_create(&recovered.reference.to_string())
            .await;
        assert_eq!(presented.issue, None);
        assert!(presented.operations[0].presented);
        assert!(restarted.create_status(None).await.operations[0].presented);

        let birth = host.join(format!("{}.birth", runtime.tmux_name));
        let original = fs::read_to_string(&birth).unwrap();
        let start = fs::read_to_string(host.join("pane-start")).unwrap();
        let mut fields: Vec<_> = original.split('~').collect();
        let start_index = fields.len() - 5;
        assert_eq!(fields[start_index], start);
        let other_start = (start.parse::<u64>().unwrap() + 1).to_string();
        fields[start_index] = &other_start;
        fs::write(birth, fields.join("~")).unwrap();
        let unavailable = restarted.create_status(None).await;
        assert_eq!(unavailable.issue, None);
        assert_eq!(unavailable.operations.len(), 1);
        let unavailable = &unavailable.operations[0];
        assert_eq!(unavailable.reference, recovered.reference);
        assert!(unavailable.presented);
        assert!(matches!(
            &unavailable.disposition,
            CreateDisposition::Launched { runtime: saved, current: PaneLiveness::Unavailable, .. }
            if saved == runtime
        ));
        let raw = restarted.inventory().await.unwrap().runtimes;
        assert_eq!(raw.len(), 1);
        assert_eq!(
            (raw[0].session_id, raw[0].runtime_id),
            (runtime.session_id, runtime.runtime_id)
        );
        assert_eq!((raw[0].workspace_id, raw[0].operation_id), (None, None));
        assert_eq!(
            fs::read_to_string(host.join("dispatched")).unwrap(),
            "marked\n"
        );
        relay.abort();
        server.abort();
        assert!(relay.await.unwrap_err().is_cancelled());
        assert!(server.await.unwrap_err().is_cancelled());
    }
}
