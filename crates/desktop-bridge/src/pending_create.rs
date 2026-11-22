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
    // Symlink rejection is also checked against the directory entry after open.
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
        if let Err(error) = DirBuilder::new().mode(0o700).create(&path) {
            if error.kind() != io::ErrorKind::AlreadyExists {
                return Err(error.into());
            }
        } else {
            File::open(directory)?.sync_all()?;
        }
        let meta = fs::symlink_metadata(&path)?;
        ensure!(
            meta.is_dir()
                && meta.mode() & 0o7777 == 0o700
                && meta.uid() == directory.metadata()?.uid(),
            "unsafe pending directory"
        );
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
        // Never replace unreadable evidence.
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
                // An unavailable connection proves this first submission never wrote a request.
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
    #[cfg(debug_assertions)]
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::{UnixListener, UnixStream},
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
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
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
            let one = Store::open(directory).unwrap().list().unwrap().0.remove(0);
            assert_eq!(one.submission, Submission::Sent);
            assert!(create_one.contains(&one.key.to_string()));
            drop(initial); // No bytes of a reply.

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
        let fixture = Fixture::new();
        let host = fixture.directory("host");
        let native = fixture.directory("native");
        let project = fixture.directory("Проєкт з пробілом");
        let tmux = host.join("tmux");
        let provider = host.join("codex");
        fs::write(&provider, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&provider, fs::Permissions::from_mode(0o700)).unwrap();
        let generation = Uuid::new_v4();
        let command = r#"#!/bin/sh
set -eu
cd "$(dirname "$3")"
token=GENERATION
case "$4" in
  display-message)
    printf '52|101|%s\n' "$token"
    if test "$#" -gt 6 && test -f name; then cat name; fi ;;
  if-shell)
    branch=$7
    case "$branch" in 'new-session '*) ;; *) exit 1 ;; esac
    case "$6" in *"$token"*) ;; *) exit 1 ;; esac
    printf 'marked\n' >> dispatched
    printf '%s\n' "$branch" | awk '{for(i=1;i<NF;i++) if ($i=="-s") {print $(i+1); exit}}' > name
    printf '%s\n' "$branch" | awk '{for(i=1;i<=NF;i++) if ($i~/^RS_CREATE_NONCE=/) {sub(/^RS_CREATE_NONCE=/,"",$i); print $i; exit}}' > nonce
    printf '$1|@2|%%3|501\n' ;;
  show-environment)
    nonce=$(cat nonce)
    printf 'RS_CREATE_NONCE=%s\n52|101|%s|$1|@2|%%3|501|%s|0|0|%s:$1:@2:%%3:501|0|100|30\n' "$nonce" "$token" "$(cat name)" "$nonce" ;;
  list-sessions) if test -f name; then cat name; fi ;;
  list-panes) printf '%s\t%%3\t501\t0\t100\t30\tcodex\n' "$(cat name)" ;;
  *) exit 1 ;;
esac
"#.replace("GENERATION", &generation.to_string());
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
