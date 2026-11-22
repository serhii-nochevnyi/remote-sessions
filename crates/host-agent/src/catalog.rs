use crate::{
    model::{
        LaunchOutcome, ProductIssue, ProductRequest, ProductResponse, ProviderProfile,
        WorkspaceDisplay, tmux_name,
    },
    product_launch::{PreparedLaunch, ServiceProfiles, WorkspaceIdentity, resolve_workspace},
};
use anyhow::{Context, Result as AnyResult, ensure};
use nix::{libc, unistd::geteuid};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

const APPLICATION_ID: i32 = 0x5253_4341;
const SCHEMA_VERSION: i32 = 2;
const BUSY_TIMEOUT_MS: u32 = 100;
const PRODUCT_TIMEOUT: Duration = Duration::from_secs(2);
const DB: &str = "catalog.sqlite3";
const RECORD: &str = "catalog.provision";
const LOCK: &str = "catalog.lock";
const SIDECARS: [&str; 3] = [
    "catalog.sqlite3-journal",
    "catalog.sqlite3-wal",
    "catalog.sqlite3-shm",
];

const META_SCHEMA: &str = "CREATE TABLE catalog_meta (singleton INTEGER PRIMARY KEY CHECK (singleton = 1), provision TEXT NOT NULL) STRICT";
const WORKSPACE_SCHEMA: &str = "CREATE TABLE workspaces (
    id TEXT PRIMARY KEY NOT NULL,
    locator TEXT NOT NULL,
    canonical TEXT NOT NULL,
    device BLOB NOT NULL CHECK (length(device) = 8),
    inode BLOB NOT NULL CHECK (length(inode) = 8),
    UNIQUE (locator, canonical, device, inode)
) STRICT";
const SESSION_SCHEMA: &str = "CREATE TABLE sessions (
    id TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL REFERENCES workspaces(id)
) STRICT";
const ATTEMPT_SCHEMA: &str = "CREATE TABLE runtime_attempts (
    id TEXT PRIMARY KEY NOT NULL,
    session_id TEXT NOT NULL UNIQUE REFERENCES sessions(id),
    target TEXT NOT NULL UNIQUE,
    nonce TEXT NOT NULL UNIQUE,
    generation TEXT NOT NULL,
    policy TEXT NOT NULL
) STRICT";
const OPERATION_SCHEMA: &str = "CREATE TABLE create_operations (
    key TEXT PRIMARY KEY NOT NULL,
    id TEXT NOT NULL UNIQUE,
    runtime_id TEXT NOT NULL UNIQUE REFERENCES runtime_attempts(id),
    profile TEXT NOT NULL,
    stage TEXT NOT NULL CHECK (stage IN ('reserved', 'marked', 'finished')),
    outcome TEXT,
    CHECK ((stage = 'finished') = (outcome IS NOT NULL))
) STRICT";

type CatalogResult<T> = Result<T, ProductIssue>;
type Identity = (u64, u64);

fn storage_error(error: io::Error) -> ProductIssue {
    match error.kind() {
        io::ErrorKind::NotFound => ProductIssue::CatalogMissing,
        _ => ProductIssue::CatalogUnavailable,
    }
}

fn database_error(error: rusqlite::Error) -> ProductIssue {
    match error.sqlite_error_code() {
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked) => {
            ProductIssue::CatalogBusy
        }
        _ => ProductIssue::CatalogUnavailable,
    }
}

fn identity(metadata: &fs::Metadata) -> Identity {
    (metadata.dev(), metadata.ino())
}

fn private_directory(dir: &Path) -> CatalogResult<Identity> {
    let metadata = fs::symlink_metadata(dir).map_err(storage_error)?;
    if !dir.is_absolute()
        || !metadata.is_dir()
        || metadata.uid() != geteuid().as_raw()
        || metadata.mode() & 0o7777 != 0o700
        || fs::canonicalize(dir).map_err(storage_error)? != dir
    {
        return Err(ProductIssue::CatalogUnsafe);
    }
    Ok(identity(&metadata))
}

fn private_file(metadata: &fs::Metadata) -> CatalogResult<Identity> {
    if !is_private_file(metadata, geteuid().as_raw()) {
        return Err(ProductIssue::CatalogUnsafe);
    }
    Ok(identity(metadata))
}

fn is_private_file(metadata: &fs::Metadata, owner: u32) -> bool {
    metadata.is_file()
        && metadata.uid() == owner
        && metadata.mode() & 0o7777 == 0o600
        && metadata.nlink() == 1
}

fn absent(path: &Path) -> CatalogResult<bool> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(storage_error(error)),
        Ok(_) => Ok(false),
    }
}

fn require_no_sidecars(dir: &Path) -> CatalogResult<()> {
    for name in SIDECARS {
        if !absent(&dir.join(name))? {
            return Err(ProductIssue::CatalogUnavailable);
        }
    }
    Ok(())
}

fn file_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    options
}

struct CatalogLock {
    file: File,
    dir: PathBuf,
    identity: Identity,
}

impl CatalogLock {
    fn acquire(dir: &Path, initializing: bool) -> CatalogResult<Self> {
        let identity = private_directory(dir)?;
        let path = dir.join(LOCK);
        if !initializing {
            private_file(&fs::symlink_metadata(&path).map_err(storage_error)?)?;
        }
        let file = file_options()
            .read(true)
            .write(true)
            .create_new(initializing)
            .open(path)
            .map_err(|error| match error.kind() {
                io::ErrorKind::AlreadyExists => ProductIssue::CatalogUnsafe,
                _ => storage_error(error),
            })?;
        private_file(&file.metadata().map_err(storage_error)?)?;
        file.try_lock().map_err(|error| match error {
            std::fs::TryLockError::WouldBlock => ProductIssue::CatalogBusy,
            std::fs::TryLockError::Error(error) => storage_error(error),
        })?;
        let lock = Self {
            file,
            dir: dir.to_owned(),
            identity,
        };
        lock.validate()?;
        Ok(lock)
    }

    fn validate(&self) -> CatalogResult<()> {
        let on_disk = fs::symlink_metadata(self.dir.join(LOCK)).map_err(storage_error)?;
        if private_file(&on_disk)? != identity(&self.file.metadata().map_err(storage_error)?)
            || private_directory(&self.dir)? != self.identity
        {
            return Err(ProductIssue::CatalogUnsafe);
        }
        Ok(())
    }
}

struct CatalogFiles {
    db: Identity,
    record: Identity,
}

impl CatalogFiles {
    fn inspect(dir: &Path) -> CatalogResult<Self> {
        require_no_sidecars(dir)?;
        let db = fs::symlink_metadata(dir.join(DB)).map_err(storage_error)?;
        let record = fs::symlink_metadata(dir.join(RECORD)).map_err(storage_error)?;
        Ok(Self {
            db: private_file(&db)?,
            record: private_file(&record)?,
        })
    }

    fn verify(&self, dir: &Path) -> CatalogResult<()> {
        let current = Self::inspect(dir)?;
        if self.db != current.db || self.record != current.record {
            return Err(ProductIssue::CatalogUnsafe);
        }
        let mut db = file_options()
            .read(true)
            .open(dir.join(DB))
            .map_err(storage_error)?;
        if private_file(&db.metadata().map_err(storage_error)?)? != self.db {
            return Err(ProductIssue::CatalogUnsafe);
        }
        let mut header = [0; 16];
        db.read_exact(&mut header)
            .map_err(|_| ProductIssue::CatalogUnavailable)?;
        if &header != b"SQLite format 3\0" {
            return Err(ProductIssue::CatalogUnavailable);
        }
        Ok(())
    }

    fn provision_id(&self, dir: &Path) -> CatalogResult<Uuid> {
        let file = file_options()
            .read(true)
            .open(dir.join(RECORD))
            .map_err(storage_error)?;
        if private_file(&file.metadata().map_err(storage_error)?)? != self.record {
            return Err(ProductIssue::CatalogUnsafe);
        }
        let mut bytes = Vec::new();
        file.take(37)
            .read_to_end(&mut bytes)
            .map_err(storage_error)?;
        Uuid::try_parse_ascii(&bytes).map_err(|_| ProductIssue::CatalogUnsafe)
    }
}

pub fn product_catalog_directory(path: &Path) -> AnyResult<PathBuf> {
    let user =
        nix::unistd::User::from_uid(geteuid())?.context("catalog requires an OS user home")?;
    let support = user.dir.join("Library/Application Support");
    ensure!(
        path.starts_with(&support)
            && path != support
            && !path
                .components()
                .any(|part| part == std::path::Component::ParentDir),
        "catalog must be below the user's Application Support directory"
    );
    let parent = path
        .parent()
        .context("catalog must have an existing parent directory")?;
    ensure!(
        fs::canonicalize(parent)? == parent,
        "catalog parent must be a real directory"
    );
    #[cfg(target_os = "macos")]
    {
        let device = Path::new("/System/Volumes/Data").metadata()?.dev();
        ensure!(
            parent.metadata()?.dev() == device,
            "catalog must be on the internal macOS data volume"
        );
        match fs::symlink_metadata(path) {
            Ok(metadata) => ensure!(
                metadata.dev() == device,
                "catalog must be on the internal macOS data volume"
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(path.to_owned())
}

pub fn initialize_catalog(dir: &Path) -> AnyResult<()> {
    let dir = product_catalog_directory(dir)?;
    provision(&dir).map_err(|issue| anyhow::anyhow!("catalog initialization blocked ({issue:?})"))
}

pub(crate) fn provision(dir: &Path) -> CatalogResult<()> {
    if absent(dir)? {
        DirBuilder::new()
            .mode(0o700)
            .create(dir)
            .map_err(storage_error)?;
    }
    private_directory(dir)?;
    require_no_sidecars(dir)?;
    if !absent(&dir.join(DB))? || !absent(&dir.join(RECORD))? {
        return Err(ProductIssue::CatalogUnsafe);
    }
    let _lock = CatalogLock::acquire(dir, true)?;

    let db = file_options()
        .write(true)
        .create_new(true)
        .open(dir.join(DB))
        .map_err(storage_error)?;
    private_file(&db.metadata().map_err(storage_error)?)?;
    let mut connection = connect(dir)?;
    configure(&connection)?;
    let provision_id = Uuid::new_v4();
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(database_error)?;
    transaction
        .pragma_update(None, "application_id", APPLICATION_ID)
        .map_err(database_error)?;
    transaction
        .execute_batch(META_SCHEMA)
        .map_err(database_error)?;
    transaction
        .execute(
            "INSERT INTO catalog_meta (singleton, provision) VALUES (1, ?1)",
            [provision_id.to_string()],
        )
        .map_err(database_error)?;
    migrate_to_v1(&transaction)?;
    migrate_to_v2(&transaction)?;
    transaction.commit().map_err(database_error)?;

    let mut record = file_options()
        .write(true)
        .create_new(true)
        .open(dir.join(RECORD))
        .map_err(storage_error)?;
    private_file(&record.metadata().map_err(storage_error)?)?;
    record
        .write_all(provision_id.to_string().as_bytes())
        .map_err(storage_error)?;
    record.sync_all().map_err(storage_error)?;
    File::open(dir)
        .and_then(|file| file.sync_all())
        .map_err(storage_error)
}

fn connect(dir: &Path) -> CatalogResult<Connection> {
    let connection = Connection::open_with_flags(
        dir.join(DB),
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(database_error)?;
    connection
        .busy_timeout(Duration::from_millis(u64::from(BUSY_TIMEOUT_MS)))
        .map_err(database_error)?;
    Ok(connection)
}

fn pragma<T: rusqlite::types::FromSql>(connection: &Connection, name: &str) -> CatalogResult<T> {
    connection
        .pragma_query_value(None, name, |row| row.get(0))
        .map_err(database_error)
}

fn configure(connection: &Connection) -> CatalogResult<()> {
    connection
        .pragma_update(None, "foreign_keys", true)
        .map_err(database_error)?;
    connection
        .pragma_update(None, "synchronous", "EXTRA")
        .map_err(database_error)?;
    connection
        .pragma_update(None, "trusted_schema", false)
        .map_err(database_error)?;
    #[cfg(target_os = "macos")]
    connection
        .pragma_update(None, "fullfsync", true)
        .map_err(database_error)?;
    if !pragma::<String>(connection, "journal_mode")?.eq_ignore_ascii_case("delete")
        || pragma::<i32>(connection, "synchronous")? != 3
        || pragma::<i32>(connection, "foreign_keys")? != 1
        || pragma::<i32>(connection, "trusted_schema")? != 0
        || pragma::<u32>(connection, "busy_timeout")? != BUSY_TIMEOUT_MS
        || i64::from(rusqlite::version_number()) != i64::from(rusqlite::ffi::SQLITE_VERSION_NUMBER)
    {
        return Err(ProductIssue::CatalogUnsupported);
    }
    #[cfg(target_os = "macos")]
    if pragma::<i32>(connection, "fullfsync")? != 1 {
        return Err(ProductIssue::CatalogUnsupported);
    }
    Ok(())
}

fn validate_schema(connection: &Connection, version: i32) -> CatalogResult<()> {
    let mut statement = connection
        .prepare("SELECT name, sql FROM sqlite_schema WHERE name NOT GLOB 'sqlite_*' ORDER BY name")
        .map_err(database_error)?;
    let actual: Vec<(String, String)> = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(database_error)?
        .collect::<rusqlite::Result<_>>()
        .map_err(database_error)?;
    let mut expected = vec![("catalog_meta", META_SCHEMA)];
    if version >= 1 {
        expected.push(("workspaces", WORKSPACE_SCHEMA));
    }
    if version >= 2 {
        expected.extend([
            ("create_operations", OPERATION_SCHEMA),
            ("runtime_attempts", ATTEMPT_SCHEMA),
            ("sessions", SESSION_SCHEMA),
        ]);
    }
    expected.sort_unstable_by_key(|(name, _)| *name);
    if !actual
        .iter()
        .map(|(name, sql)| (name.as_str(), sql.as_str()))
        .eq(expected)
    {
        return Err(ProductIssue::CatalogUnsupported);
    }
    Ok(())
}

fn migrate_to_v1(connection: &Connection) -> CatalogResult<()> {
    connection
        .execute_batch(WORKSPACE_SCHEMA)
        .map_err(database_error)?;
    connection
        .pragma_update(None, "user_version", 1)
        .map_err(database_error)
}

fn migrate_to_v2(connection: &Connection) -> CatalogResult<()> {
    for schema in [SESSION_SCHEMA, ATTEMPT_SCHEMA, OPERATION_SCHEMA] {
        connection.execute_batch(schema).map_err(database_error)?;
    }
    connection
        .pragma_update(None, "user_version", 2)
        .map_err(database_error)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ServerGeneration {
    pub pid: u32,
    pub start: u64,
    pub token: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PaneIdentity {
    pub session: String,
    pub window: String,
    pub pane: String,
    pub pid: u32,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum StoredOutcome {
    Failed(ProductIssue),
    Launched {
        identity: PaneIdentity,
        outcome: Box<LaunchOutcome>,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OperationStage {
    Reserved,
    Marked,
    Finished(StoredOutcome),
}

#[derive(Debug)]
pub(crate) struct Operation {
    pub key: Uuid,
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub profile: ProviderProfile,
    pub session_id: Uuid,
    pub runtime_id: Uuid,
    pub target: String,
    pub nonce: Uuid,
    pub generation: ServerGeneration,
    pub locator: String,
    pub policy: PreparedLaunch,
    pub stage: OperationStage,
}

const OPERATION_QUERY: &str = "SELECT o.key, o.id, s.workspace_id, o.profile, s.id, a.id,
    a.target, a.nonce, a.generation, w.locator, a.policy, o.stage, o.outcome
    FROM create_operations o JOIN runtime_attempts a ON a.id = o.runtime_id
    JOIN sessions s ON s.id = a.session_id JOIN workspaces w ON w.id = s.workspace_id";

fn json<T: Serialize>(value: &T) -> CatalogResult<String> {
    serde_json::to_string(value).map_err(|_| ProductIssue::CatalogUnavailable)
}

fn read_operation(row: &rusqlite::Row<'_>) -> rusqlite::Result<Operation> {
    fn invalid(
        index: usize,
        error: impl std::error::Error + Send + Sync + 'static,
    ) -> rusqlite::Error {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    }
    fn uuid(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Uuid> {
        Uuid::try_parse(&row.get::<_, String>(index)?).map_err(|error| invalid(index, error))
    }
    fn decode<T: DeserializeOwned>(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<T> {
        serde_json::from_str(&row.get::<_, String>(index)?).map_err(|error| invalid(index, error))
    }
    let session_id = uuid(row, 4)?;
    let runtime_id = uuid(row, 5)?;
    let target: String = row.get(6)?;
    if target != tmux_name(session_id, runtime_id) {
        return Err(invalid(6, io::Error::other("invalid reserved tmux target")));
    }
    let operation = Operation {
        key: uuid(row, 0)?,
        id: uuid(row, 1)?,
        workspace_id: uuid(row, 2)?,
        profile: decode(row, 3)?,
        session_id,
        runtime_id,
        target,
        nonce: uuid(row, 7)?,
        generation: decode(row, 8)?,
        locator: row.get(9)?,
        policy: decode(row, 10)?,
        stage: match row.get::<_, String>(11)?.as_str() {
            "reserved" => OperationStage::Reserved,
            "marked" => OperationStage::Marked,
            "finished" => OperationStage::Finished(decode(row, 12)?),
            _ => return Err(invalid(11, io::Error::other("invalid launch stage"))),
        },
    };
    if let OperationStage::Finished(StoredOutcome::Launched { identity, outcome }) =
        &operation.stage
    {
        let runtime = &outcome.runtime;
        let pid_matches = match runtime.pane_pid {
            Some(pid) => pid == identity.pid,
            None => runtime.exited,
        };
        if (runtime.session_id, runtime.runtime_id) != (operation.session_id, operation.runtime_id)
            || runtime.tmux_name != operation.target
            || runtime.pane_id != identity.pane
            || identity.pid == 0
            || !pid_matches
            || runtime.workspace_id != Some(operation.workspace_id)
            || runtime.operation_id != Some(operation.id)
        {
            return Err(invalid(
                12,
                io::Error::other("outcome identity differs from reservation"),
            ));
        }
    }
    Ok(operation)
}

pub(crate) struct Catalog {
    connection: Connection,
    lock: CatalogLock,
    files: CatalogFiles,
    provision_id: Uuid,
}

impl Catalog {
    #[cfg(test)]
    pub(crate) fn connection_for_test(&self) -> &Connection {
        &self.connection
    }

    fn open(dir: &Path) -> CatalogResult<Self> {
        let lock = CatalogLock::acquire(dir, false)?;
        let files = CatalogFiles::inspect(dir)?;
        let provision_id = files.provision_id(dir)?;
        let mut connection = connect(dir)?;
        if pragma::<i32>(&connection, "application_id")? != APPLICATION_ID {
            return Err(ProductIssue::CatalogUnsupported);
        }
        let version = pragma::<i32>(&connection, "user_version")?;
        if !(0..=SCHEMA_VERSION).contains(&version) {
            return Err(ProductIssue::CatalogUnsupported);
        }
        configure(&connection)?;
        validate_schema(&connection, version)?;
        let record: String = connection
            .query_row(
                "SELECT provision FROM catalog_meta WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .map_err(database_error)?;
        if Uuid::try_parse(&record).ok() != Some(provision_id) {
            return Err(ProductIssue::CatalogUnsafe);
        }
        let quick_check: String = connection
            .query_row("PRAGMA quick_check(1)", [], |row| row.get(0))
            .map_err(database_error)?;
        if quick_check != "ok" {
            return Err(ProductIssue::CatalogUnavailable);
        }
        files.verify(dir)?;
        if version < SCHEMA_VERSION {
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(database_error)?;
            if version == 0 {
                migrate_to_v1(&transaction)?;
            }
            migrate_to_v2(&transaction)?;
            validate_schema(&transaction, SCHEMA_VERSION)?;
            transaction.commit().map_err(database_error)?;
        }
        Ok(Self {
            connection,
            lock,
            files,
            provision_id,
        })
    }

    pub(crate) fn operation(&self, key: Uuid) -> CatalogResult<Option<Operation>> {
        self.connection
            .query_row(
                &format!("{OPERATION_QUERY} WHERE o.key = ?1"),
                [key.to_string()],
                read_operation,
            )
            .optional()
            .map_err(database_error)
    }

    pub(crate) fn by_runtime(&self, ids: &[Uuid]) -> CatalogResult<Vec<Operation>> {
        let mut query = self
            .connection
            .prepare(&format!(
                "{OPERATION_QUERY} WHERE a.id = ?1 AND o.stage = 'finished'"
            ))
            .map_err(database_error)?;
        ids.iter()
            .filter_map(|id| {
                query
                    .query_row([id.to_string()], read_operation)
                    .optional()
                    .transpose()
            })
            .collect::<rusqlite::Result<_>>()
            .map_err(database_error)
    }

    pub(crate) fn marked_targets(
        &self,
        generation: ServerGeneration,
    ) -> CatalogResult<Vec<String>> {
        let mut query = self.connection.prepare(
            "SELECT a.target FROM runtime_attempts a JOIN create_operations o ON o.runtime_id = a.id
             WHERE o.stage = 'marked' AND a.generation = ?1",
        ).map_err(database_error)?;
        query
            .query_map([json(&generation)?], |row| row.get(0))
            .map_err(database_error)?
            .collect::<rusqlite::Result<_>>()
            .map_err(database_error)
    }

    pub(crate) fn reserve(&mut self, operation: &Operation) -> CatalogResult<bool> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database_error)?;
        if tx
            .query_row(
                "SELECT 1 FROM create_operations WHERE key = ?1",
                [operation.key.to_string()],
                |_| Ok(()),
            )
            .optional()
            .map_err(database_error)?
            .is_some()
        {
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO sessions (id, workspace_id) VALUES (?1, ?2)",
            params![
                operation.session_id.to_string(),
                operation.workspace_id.to_string()
            ],
        )
        .map_err(database_error)?;
        tx.execute(
            "INSERT INTO runtime_attempts (id, session_id, target, nonce, generation, policy)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                operation.runtime_id.to_string(),
                operation.session_id.to_string(),
                operation.target,
                operation.nonce.to_string(),
                json(&operation.generation)?,
                json(&operation.policy)?
            ],
        )
        .map_err(database_error)?;
        tx.execute(
            "INSERT INTO create_operations (key, id, runtime_id, profile, stage)
            VALUES (?1, ?2, ?3, ?4, 'reserved')",
            params![
                operation.key.to_string(),
                operation.id.to_string(),
                operation.runtime_id.to_string(),
                json(&operation.profile)?
            ],
        )
        .map_err(database_error)?;
        tx.commit().map_err(database_error)?;
        Ok(true)
    }

    pub(crate) fn preflight(
        &self,
        id: Uuid,
        profile: ProviderProfile,
        profiles: &ServiceProfiles,
    ) -> CatalogResult<(String, PreparedLaunch)> {
        let (locator, identity) = self.workspace(id)?;
        let policy = profiles.preflight(Path::new(&locator), &identity, profile)?;
        Ok((locator, policy))
    }

    pub(crate) fn mark(&mut self, key: Uuid, runtime_id: Uuid) -> CatalogResult<bool> {
        self.advance(key, runtime_id, "reserved", "marked", None)
    }

    pub(crate) fn finalize(
        &mut self,
        key: Uuid,
        runtime_id: Uuid,
        outcome: &StoredOutcome,
    ) -> CatalogResult<bool> {
        let from = match outcome {
            StoredOutcome::Failed(_) => "reserved",
            StoredOutcome::Launched { .. } => "marked",
        };
        self.advance(key, runtime_id, from, "finished", Some(json(outcome)?))
    }

    fn advance(
        &mut self,
        key: Uuid,
        runtime_id: Uuid,
        from: &str,
        to: &str,
        outcome: Option<String>,
    ) -> CatalogResult<bool> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database_error)?;
        let changed = tx
            .execute(
                "UPDATE create_operations SET stage = ?1, outcome = ?2
             WHERE key = ?3 AND runtime_id = ?4 AND stage = ?5",
                params![to, outcome, key.to_string(), runtime_id.to_string(), from],
            )
            .map_err(database_error)?;
        tx.commit().map_err(database_error)?;
        Ok(changed == 1)
    }

    fn register(
        &mut self,
        locator: String,
        workspace: WorkspaceIdentity,
    ) -> CatalogResult<WorkspaceDisplay> {
        let path = Path::new(&locator);
        let label = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("/")
            .to_owned();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database_error)?;
        let device = workspace.device.to_be_bytes();
        let inode = workspace.inode.to_be_bytes();
        transaction.execute(
            "INSERT INTO workspaces (id, locator, canonical, device, inode) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (locator, canonical, device, inode) DO NOTHING",
            params![Uuid::new_v4().to_string(), locator, workspace.canonical, device, inode],
        ).map_err(database_error)?;
        let id: String = transaction.query_row(
            "SELECT id FROM workspaces WHERE locator = ?1 AND canonical = ?2 AND device = ?3 AND inode = ?4",
            params![locator, workspace.canonical, device, inode],
            |row| row.get(0),
        ).map_err(database_error)?;
        let workspace_id = Uuid::try_parse(&id).map_err(|_| ProductIssue::CatalogUnavailable)?;
        transaction.commit().map_err(database_error)?;
        Ok(WorkspaceDisplay {
            workspace_id,
            label,
        })
    }

    fn workspace(&self, id: Uuid) -> CatalogResult<(String, WorkspaceIdentity)> {
        self.connection
            .query_row(
                "SELECT locator, canonical, device, inode FROM workspaces WHERE id = ?1",
                [id.to_string()],
                |row| {
                    Ok((
                        row.get(0)?,
                        WorkspaceIdentity {
                            canonical: row.get(1)?,
                            device: u64::from_be_bytes(row.get(2)?),
                            inode: u64::from_be_bytes(row.get(3)?),
                        },
                    ))
                },
            )
            .optional()
            .map_err(database_error)?
            .ok_or(ProductIssue::WorkspaceUnknown)
    }

    fn validate_files(&self) -> CatalogResult<()> {
        self.lock.validate()?;
        self.files.verify(&self.lock.dir)?;
        if self.files.provision_id(&self.lock.dir)? != self.provision_id {
            return Err(ProductIssue::CatalogUnsafe);
        }
        Ok(())
    }

    fn handle(
        &mut self,
        request: ProductRequest,
        profiles: &ServiceProfiles,
        cancelled: impl Fn() -> bool,
    ) -> CatalogResult<ProductResponse> {
        match request {
            ProductRequest::RegisterWorkspace { locator } => {
                let identity = resolve_workspace(Path::new(&locator))?;
                self.validate_files()?;
                if cancelled() {
                    return Err(ProductIssue::WorkspaceCheckTimedOut);
                }
                let workspace = self.register(locator, identity)?;
                Ok(ProductResponse::WorkspaceRegistered { workspace })
            }
            ProductRequest::WorkspaceReadiness {
                workspace_id,
                profile,
            } => {
                self.preflight(workspace_id, profile, profiles)?;
                Ok(ProductResponse::Ready {})
            }
            ProductRequest::CreateManaged { .. } | ProductRequest::CreateStatus { .. } => {
                Err(ProductIssue::InvalidRequest)
            }
        }
    }
}

type Job = Box<dyn FnOnce(&mut CatalogResult<Catalog>, &ServiceProfiles) + Send>;

#[derive(Clone)]
pub(crate) struct CatalogLane {
    jobs: mpsc::Sender<Job>,
}

impl CatalogLane {
    pub(crate) fn start(
        directory: impl FnOnce() -> AnyResult<PathBuf> + Send + 'static,
        profiles: ServiceProfiles,
    ) -> Self {
        let (jobs, mut queue) = mpsc::channel::<Job>(4);
        if let Err(error) = std::thread::Builder::new()
            .name("host-catalog".into())
            .spawn(move || {
                let mut catalog = directory()
                    .map_err(|error| {
                        eprintln!("local catalog configuration: {error:#}");
                        ProductIssue::CatalogUnsafe
                    })
                    .and_then(|directory| Catalog::open(&directory));
                match &catalog {
                    Ok(catalog) => eprintln!(
                        "local catalog ready (SQLite {}, schema {}, DELETE, EXTRA, foreign keys on, fullfsync {}, busy timeout {} ms, device {}, owner {})",
                        rusqlite::version(), SCHEMA_VERSION, cfg!(target_os = "macos"),
                        BUSY_TIMEOUT_MS, catalog.lock.identity.0, geteuid().as_raw(),
                    ),
                    Err(issue) => eprintln!("local catalog unavailable ({issue:?})"),
                }
                while let Some(job) = queue.blocking_recv() {
                    job(&mut catalog, &profiles);
                }
            })
        {
            eprintln!("local catalog worker unavailable: {error}");
        }
        Self { jobs }
    }

    pub(crate) async fn call(&self, request: ProductRequest) -> ProductResponse {
        self.submit(move |catalog, profiles, reply| {
            catalog.handle(request, profiles, || reply.is_closed())
        })
        .await
        .unwrap_or_else(|issue| ProductResponse::Blocked { issue })
    }

    pub(crate) async fn run<T: Send + 'static>(
        &self,
        action: impl FnOnce(&mut Catalog, &ServiceProfiles) -> CatalogResult<T> + Send + 'static,
    ) -> CatalogResult<T> {
        self.submit(move |catalog, profiles, _| action(catalog, profiles))
            .await
    }

    async fn submit<T: Send + 'static>(
        &self,
        action: impl FnOnce(
            &mut Catalog,
            &ServiceProfiles,
            &oneshot::Sender<CatalogResult<T>>,
        ) -> CatalogResult<T>
        + Send
        + 'static,
    ) -> CatalogResult<T> {
        let (reply, response) = oneshot::channel();
        let job = Box::new(
            move |slot: &mut CatalogResult<Catalog>, profiles: &ServiceProfiles| {
                if reply.is_closed() {
                    return;
                }
                let result = match slot {
                    Ok(catalog) => catalog
                        .validate_files()
                        .and_then(|()| action(catalog, profiles, &reply))
                        .and_then(|value| {
                            if !catalog.connection.is_autocommit() {
                                return Err(ProductIssue::CatalogUnavailable);
                            }
                            catalog.validate_files()?;
                            Ok(value)
                        }),
                    Err(issue) => Err(*issue),
                };
                if let Err(
                    issue @ (ProductIssue::CatalogUnsafe
                    | ProductIssue::CatalogMissing
                    | ProductIssue::CatalogUnsupported
                    | ProductIssue::CatalogUnavailable),
                ) = &result
                {
                    *slot = Err(*issue);
                }
                let _ = reply.send(result);
            },
        );
        match self.jobs.try_send(job) {
            Ok(()) => match tokio::time::timeout(PRODUCT_TIMEOUT, response).await {
                Ok(Ok(response)) => response,
                Ok(Err(_)) => Err(ProductIssue::CatalogUnavailable),
                Err(_) => Err(ProductIssue::WorkspaceCheckTimedOut),
            },
            Err(mpsc::error::TrySendError::Full(_)) => Err(ProductIssue::CatalogBusy),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(ProductIssue::CatalogUnavailable),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let dir = temp.path().canonicalize().unwrap().join("catalog");
        provision(&dir).unwrap();
        (temp, dir)
    }

    #[test]
    fn w1_readiness_persists_workspace_identity_and_checks_database_policy() {
        let (temp, dir) = fixture();
        let path = temp.path().canonicalize().unwrap().join("Project Folder");
        fs::create_dir(&path).unwrap();
        let locator = path.to_str().unwrap().to_owned();
        let mut opened = Catalog::open(&dir).unwrap();
        assert_eq!(Catalog::open(&dir).err(), Some(ProductIssue::CatalogBusy));
        let connection = &opened.connection;
        assert_eq!(
            pragma::<i32>(connection, "application_id").unwrap(),
            APPLICATION_ID
        );
        assert_eq!(
            pragma::<i32>(connection, "user_version").unwrap(),
            SCHEMA_VERSION
        );
        assert_eq!(
            pragma::<String>(connection, "journal_mode").unwrap(),
            "delete"
        );
        assert_eq!(pragma::<i32>(connection, "synchronous").unwrap(), 3);
        assert_eq!(pragma::<i32>(connection, "foreign_keys").unwrap(), 1);
        assert_eq!(
            pragma::<u32>(connection, "busy_timeout").unwrap(),
            BUSY_TIMEOUT_MS
        );
        #[cfg(target_os = "macos")]
        assert_eq!(pragma::<i32>(connection, "fullfsync").unwrap(), 1);
        assert_eq!(
            i64::from(rusqlite::version_number()),
            i64::from(rusqlite::ffi::SQLITE_VERSION_NUMBER)
        );
        let identity = resolve_workspace(&path).unwrap();
        let first = opened.register(locator.clone(), identity).unwrap();
        drop(opened);

        let mut reopened = Catalog::open(&dir).unwrap();
        let (saved_locator, saved_identity) = reopened.workspace(first.workspace_id).unwrap();
        assert_eq!(saved_locator, locator);
        assert_eq!(saved_identity, resolve_workspace(&path).unwrap());
        assert_eq!(reopened.register(locator, saved_identity).unwrap(), first);
    }

    #[tokio::test]
    async fn w1_readiness_stalled_catalog_reply_is_bounded() {
        let (jobs, mut queue) = mpsc::channel(1);
        let lane = CatalogLane { jobs };
        let request = ProductRequest::WorkspaceReadiness {
            workspace_id: Uuid::new_v4(),
            profile: crate::model::ProviderProfile::Codex,
        };
        let (response, job) = tokio::join!(lane.call(request), queue.recv());
        assert_eq!(
            response,
            ProductResponse::Blocked {
                issue: ProductIssue::WorkspaceCheckTimedOut,
            }
        );
        let mut unavailable = Err(ProductIssue::CatalogBusy);
        job.unwrap()(&mut unavailable, &ServiceProfiles::new(None));
    }

    #[test]
    fn w1_readiness_expired_registration_does_not_write() {
        let (temp, dir) = fixture();
        let mut catalog = Catalog::open(&dir).unwrap();
        let (reply, response) = oneshot::channel::<()>();
        drop(response);
        let request = ProductRequest::RegisterWorkspace {
            locator: temp
                .path()
                .canonicalize()
                .unwrap()
                .into_os_string()
                .into_string()
                .unwrap(),
        };
        assert_eq!(
            catalog.handle(request, &ServiceProfiles::new(None), || reply.is_closed()),
            Err(ProductIssue::WorkspaceCheckTimedOut)
        );
        let count: i64 = catalog
            .connection
            .query_row("SELECT COUNT(*) FROM workspaces", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn w1_readiness_unsafe_missing_and_unsupported_catalogs_preserve_files() {
        let (temp, dir) = fixture();
        let alias = temp.path().join("alias");
        symlink(&dir, &alias).unwrap();
        assert_eq!(
            Catalog::open(&alias).err(),
            Some(ProductIssue::CatalogUnsafe)
        );
        let db = dir.join(DB);
        let metadata = fs::metadata(&db).unwrap();
        assert!(is_private_file(&metadata, metadata.uid()));
        assert!(!is_private_file(&metadata, metadata.uid().wrapping_add(1)));
        let backup = dir.join("backup");
        fs::rename(&db, &backup).unwrap();
        assert_eq!(
            Catalog::open(&dir).err(),
            Some(ProductIssue::CatalogMissing)
        );
        assert_eq!(provision(&dir), Err(ProductIssue::CatalogUnsafe));
        assert!(!db.exists());
        symlink(&backup, &db).unwrap();
        assert_eq!(Catalog::open(&dir).err(), Some(ProductIssue::CatalogUnsafe));
        fs::remove_file(&db).unwrap();
        fs::rename(&backup, &db).unwrap();
        fs::set_permissions(&db, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(Catalog::open(&dir).err(), Some(ProductIssue::CatalogUnsafe));
        fs::set_permissions(&db, fs::Permissions::from_mode(0o600)).unwrap();

        let record = dir.join(RECORD);
        let saved = fs::read(&record).unwrap();
        fs::remove_file(&record).unwrap();
        assert_eq!(
            Catalog::open(&dir).err(),
            Some(ProductIssue::CatalogMissing)
        );
        let mut file = file_options()
            .create_new(true)
            .write(true)
            .open(&record)
            .unwrap();
        file.write_all(Uuid::new_v4().to_string().as_bytes())
            .unwrap();
        assert_eq!(Catalog::open(&dir).err(), Some(ProductIssue::CatalogUnsafe));
        fs::write(&record, saved).unwrap();

        let journal = dir.join(SIDECARS[0]);
        fs::write(&journal, "retain rollback evidence").unwrap();
        let before = fs::read(&db).unwrap();
        assert_eq!(
            Catalog::open(&dir).err(),
            Some(ProductIssue::CatalogUnavailable)
        );
        assert_eq!(
            fs::read_to_string(&journal).unwrap(),
            "retain rollback evidence"
        );
        assert_eq!(fs::read(&db).unwrap(), before);
        fs::remove_file(&journal).unwrap();

        let connection = connect(&dir).unwrap();
        connection
            .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        drop(connection);
        let before = fs::read(&db).unwrap();
        assert_eq!(
            Catalog::open(&dir).err(),
            Some(ProductIssue::CatalogUnsupported)
        );
        assert_eq!(fs::read(&db).unwrap(), before);
        fs::rename(&db, &backup).unwrap();
        fs::remove_file(&record).unwrap();
        assert_eq!(
            Catalog::open(&dir).err(),
            Some(ProductIssue::CatalogMissing)
        );
        assert_eq!(provision(&dir), Err(ProductIssue::CatalogUnsafe));
        assert!(!db.exists() && !record.exists());
    }

    #[test]
    fn w1_create_migration_rollback_then_upgrade_preserves_workspace_identity() {
        let (_temp, dir) = fixture();
        let workspace = dir.parent().unwrap();
        let locator = workspace.to_str().unwrap().to_owned();
        let mut opened = Catalog::open(&dir).unwrap();
        let original = opened
            .register(locator.clone(), resolve_workspace(workspace).unwrap())
            .unwrap();
        drop(opened);
        let mut reader = connect(&dir).unwrap();
        reader
            .execute_batch(
                "DROP TABLE create_operations; DROP TABLE runtime_attempts;
                DROP TABLE sessions; PRAGMA user_version = 1;",
            )
            .unwrap();
        let before = fs::read(dir.join(DB)).unwrap();
        let snapshot = reader.transaction().unwrap();
        assert_eq!(pragma::<i32>(&snapshot, "user_version").unwrap(), 1);
        assert_eq!(Catalog::open(&dir).err(), Some(ProductIssue::CatalogBusy));
        snapshot.rollback().unwrap();
        assert_eq!(pragma::<i32>(&reader, "user_version").unwrap(), 1);
        assert_eq!(fs::read(dir.join(DB)).unwrap(), before);
        validate_schema(&reader, 1).unwrap();
        drop(reader);
        let reopened = Catalog::open(&dir).unwrap();
        assert_eq!(
            reopened.workspace(original.workspace_id).unwrap().0,
            locator
        );
        assert_eq!(
            pragma::<i32>(&reopened.connection, "user_version").unwrap(),
            SCHEMA_VERSION
        );
    }
}
