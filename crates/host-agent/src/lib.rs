//! Local-only P0 runtime adapter. Network transport is absent; local input requires an exclusive lease.
mod catalog;
pub mod engine;
pub mod model;
pub mod ownership;
pub mod product_launch;
#[cfg(test)]
mod w1_create_tests;

use anyhow::{Context, Result, ensure};
use catalog::CatalogLane;
use engine::Engine;
use model::{ProductEnvelope, ProductIssue, ProductRequest, ProductResponse, Request, Response};
use nix::unistd::geteuid;
use product_launch::ServiceProfiles;
use serde::{Serialize, de::DeserializeOwned};
use std::{
    fs::{self, File, OpenOptions},
    os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{Mutex, Semaphore},
    task::JoinSet,
};

pub use catalog::{initialize_catalog, product_catalog_directory};

const REQUEST_LIMIT: usize = 65_536;
const RESPONSE_LIMIT: usize = 1_048_576;
const IO_TIMEOUT: Duration = Duration::from_secs(5);

pub fn validate_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).context("create a private state directory first")?;
    ensure!(
        path.is_absolute() && path.as_os_str().len() <= 80,
        "state directory must be absolute and at most 80 bytes for Unix sockets"
    );
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "state directory must not be a symlink"
    );
    ensure!(
        metadata.uid() == geteuid().as_raw() && metadata.mode() & 0o077 == 0,
        "state directory must be owned by this user with mode 0700"
    );
    Ok(())
}

struct AgentLock {
    _file: File,
    socket: PathBuf,
}
impl AgentLock {
    fn acquire(directory: &Path) -> Result<Self> {
        validate_directory(directory)?;
        let path = directory.join("agent.lock");
        if let Ok(meta) = fs::symlink_metadata(&path) {
            ensure!(
                meta.is_file()
                    && !meta.file_type().is_symlink()
                    && meta.uid() == geteuid().as_raw(),
                "invalid agent lock file"
            );
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        file.try_lock()
            .context("another agent already owns this state directory")?;
        let socket = directory.join("agent.sock");
        if let Ok(meta) = fs::symlink_metadata(&socket) {
            ensure!(
                meta.file_type().is_socket() && meta.uid() == geteuid().as_raw(),
                "refusing to replace a non-socket agent path"
            );
            fs::remove_file(&socket)?;
        }
        Ok(Self {
            _file: file,
            socket,
        })
    }
}
impl Drop for AgentLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket);
    }
}

async fn read_frame<R: AsyncRead + Unpin>(stream: &mut R, limit: usize) -> Result<Vec<u8>> {
    let mut frame = Vec::new();
    let mut reader = BufReader::new(stream.take((limit + 1) as u64));
    tokio::time::timeout(IO_TIMEOUT, reader.read_until(b'\n', &mut frame))
        .await
        .context("IPC read timeout")??;
    ensure!(
        frame.len() <= limit && frame.last() == Some(&b'\n'),
        "IPC frame is incomplete or exceeds limit"
    );
    Ok(frame)
}

pub async fn request(directory: &Path, request: &Request) -> Result<Response> {
    exchange(directory, request).await
}

pub async fn product_request(
    directory: &Path,
    product: &ProductRequest,
) -> Result<ProductResponse> {
    let reply: ProductEnvelope<ProductResponse> =
        exchange(directory, &ProductEnvelope { product }).await?;
    Ok(reply.product)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateDeliveryError {
    BeforeSend,
    Uncertain,
}

pub async fn submit_product_create(
    directory: &Path,
    product: &ProductRequest,
) -> std::result::Result<ProductResponse, CreateDeliveryError> {
    let stream = connect_agent(directory)
        .await
        .map_err(|_| CreateDeliveryError::BeforeSend)?;
    let reply: ProductEnvelope<ProductResponse> = exchange_on(stream, &ProductEnvelope { product })
        .await
        .map_err(|_| CreateDeliveryError::Uncertain)?;
    Ok(reply.product)
}

async fn connect_agent(directory: &Path) -> Result<UnixStream> {
    validate_directory(directory)?;
    let stream = tokio::time::timeout(
        IO_TIMEOUT,
        UnixStream::connect(directory.join("agent.sock")),
    )
    .await?
    .context("local agent is unavailable")?;
    ensure!(
        stream.peer_cred()?.uid() == geteuid().as_raw(),
        "unexpected agent UID"
    );
    Ok(stream)
}

async fn exchange<T: Serialize, R: DeserializeOwned>(directory: &Path, request: &T) -> Result<R> {
    exchange_on(connect_agent(directory).await?, request).await
}

async fn exchange_on<T: Serialize, R: DeserializeOwned>(
    mut stream: UnixStream,
    request: &T,
) -> Result<R> {
    let mut bytes = serde_json::to_vec(request)?;
    bytes.push(b'\n');
    ensure!(bytes.len() <= REQUEST_LIMIT, "request too large");
    tokio::time::timeout(IO_TIMEOUT, stream.write_all(&bytes)).await??;
    let result = read_frame(&mut stream, RESPONSE_LIMIT).await?;
    serde_json::from_slice(&result).context("invalid agent response")
}

pub async fn serve(directory: PathBuf, tmux: PathBuf) -> Result<()> {
    serve_local(directory, tmux, None).await
}

pub async fn serve_with_catalog(directory: PathBuf, tmux: PathBuf, catalog: PathBuf) -> Result<()> {
    let service_path = std::env::var_os("PATH");
    let lane = CatalogLane::start(
        move || product_catalog_directory(&catalog),
        ServiceProfiles::new(service_path.as_deref()),
    );
    serve_local(directory, tmux, Some(lane)).await
}

/// Runs the same dispatcher and SQLite ledger in an isolated development fixture.
#[cfg(debug_assertions)]
#[doc(hidden)]
pub async fn serve_catalog_fixture(
    directory: PathBuf,
    tmux: PathBuf,
    catalog: PathBuf,
    service_path: &std::ffi::OsStr,
) -> Result<()> {
    catalog::provision(&catalog).map_err(|issue| anyhow::anyhow!("fixture catalog: {issue:?}"))?;
    let lane = CatalogLane::start(
        move || Ok(catalog),
        ServiceProfiles::new(Some(service_path)),
    );
    serve_local(directory, tmux, Some(lane)).await
}

async fn dispatch(
    bytes: &[u8],
    engine: &Mutex<Engine>,
    catalog: Option<&CatalogLane>,
) -> Result<Vec<u8>> {
    if serde_json::from_slice::<serde_json::Value>(bytes)?
        .get("product")
        .is_some()
    {
        let product = match serde_json::from_slice::<ProductEnvelope<ProductRequest>>(bytes) {
            Ok(envelope) => match (catalog, envelope.product) {
                (
                    Some(lane),
                    ProductRequest::CreateManaged {
                        key,
                        workspace_id,
                        profile,
                        reservation,
                    },
                ) => {
                    let state = engine
                        .lock()
                        .await
                        .create(lane, key, workspace_id, profile, reservation)
                        .await;
                    ProductResponse::Create { state }
                }
                (Some(lane), ProductRequest::CreateStatus { key }) => {
                    let state = engine.lock().await.create_status(lane, key).await;
                    ProductResponse::Create { state }
                }
                (Some(lane), product) => lane.call(product).await,
                (None, _) => ProductResponse::Blocked {
                    issue: ProductIssue::CatalogUnconfigured,
                },
            },
            Err(_) => ProductResponse::Blocked {
                issue: ProductIssue::InvalidRequest,
            },
        };
        return Ok(serde_json::to_vec(&ProductEnvelope { product })?);
    }
    let request = serde_json::from_slice::<Request>(bytes)?;
    let mut engine = engine.lock().await;
    let response = match request {
        Request::List {} => Response::Runtimes {
            runtimes: engine.inventory(catalog).await?,
        },
        request => engine.handle(request).await?,
    };
    Ok(serde_json::to_vec(&response)?)
}

async fn serve_local(
    directory: PathBuf,
    tmux: PathBuf,
    catalog: Option<CatalogLane>,
) -> Result<()> {
    let guard = AgentLock::acquire(&directory)?;
    let listener = UnixListener::bind(&guard.socket)?;
    fs::set_permissions(&guard.socket, fs::Permissions::from_mode(0o600))?;
    let engine = Arc::new(Mutex::new(Engine {
        tmux,
        socket: directory.join("tmux.sock"),
        ownership: Default::default(),
    }));
    let permits = Arc::new(Semaphore::new(16));
    let mut jobs = JoinSet::new();
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    eprintln!("local runtime spike listening; remote execution disabled");
    loop {
        tokio::select! {
            _ = term.recv() => break,
            _ = tokio::signal::ctrl_c() => break,
            Some(_) = jobs.join_next(), if !jobs.is_empty() => {},
            accepted = listener.accept() => {
                let (mut stream, _) = accepted?;
                if !stream.peer_cred().is_ok_and(|peer| peer.uid() == geteuid().as_raw()) { continue; }
                let Ok(permit) = permits.clone().try_acquire_owned() else { continue; };
                let engine = engine.clone();
                let catalog = catalog.clone();
                jobs.spawn(async move {
                    let _permit = permit;
                    let result: Result<Vec<u8>> = async {
                        let bytes = read_frame(&mut stream, REQUEST_LIMIT).await?;
                        dispatch(&bytes, &engine, catalog.as_ref()).await
                    }.await;
                    let response = result.or_else(|error| serde_json::to_vec(&Response::Error { message: error.to_string() }));
                    if let Ok(mut bytes) = response && bytes.len() < RESPONSE_LIMIT {
                        bytes.push(b'\n');
                        let _ = tokio::time::timeout(IO_TIMEOUT, stream.write_all(&bytes)).await;
                    }
                });
            }
        }
    }
    jobs.abort_all();
    while jobs.join_next().await.is_some() {}
    // Deliberately no tmux/session stop: the separate service owns all runtimes.
    drop(guard);
    Ok(())
}

pub fn find_executable(name: &str) -> Option<PathBuf> {
    let candidate = Path::new(name);
    let executable = |path: &Path| {
        path.metadata()
            .is_ok_and(|m| m.is_file() && m.mode() & 0o111 != 0)
    };
    if candidate.is_absolute() {
        return executable(candidate).then(|| candidate.to_path_buf());
    }
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|p| p.join(name))
            .find(|p| executable(p))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ProviderProfile, tmux_name};
    use std::os::unix::{ffi::OsStringExt, fs::symlink};
    use uuid::Uuid;
    #[test]
    fn only_one_agent_can_claim_a_private_directory() {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let first = AgentLock::acquire(dir.path()).unwrap();
        assert!(AgentLock::acquire(dir.path()).is_err());
        drop(first);
        assert!(AgentLock::acquire(dir.path()).is_ok());
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(validate_directory(dir.path()).is_err());
    }
    #[tokio::test]
    async fn ipc_rejects_oversized_and_unterminated_frames() {
        let mut oversized = &b"123456\n"[..];
        assert!(read_frame(&mut oversized, 4).await.is_err());
        let mut partial = &b"{}"[..];
        assert!(read_frame(&mut partial, 10).await.is_err());
    }

    pub(super) fn fixture_program(path: &Path, script: &str) {
        fs::write(path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[tokio::test]
    async fn w1_readiness_private_ipc_preserves_inventory_and_never_launches() {
        let temp = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in("/tmp")
            .unwrap();
        let root = &temp.path().canonicalize().unwrap();
        let catalog_dir = root.join("catalog");
        catalog::provision(&catalog_dir).unwrap();
        let workspace = root.join("Проєкт з пробілами");
        fs::create_dir(&workspace).unwrap();
        let selected = root.join("Вибрана папка");
        symlink(&workspace, &selected).unwrap();
        let denied = root.join("denied");
        fs::create_dir(&denied).unwrap();
        fs::set_permissions(&denied, fs::Permissions::from_mode(0o000)).unwrap();
        let provider_runs = root.join("provider-ran");
        let script = format!("#!/bin/sh\ntouch '{}'\n", provider_runs.display());
        fixture_program(&root.join("claude"), &script);
        fixture_program(&root.join("node"), &script);
        fixture_program(&root.join("codex"), "#!/usr/bin/env node\n");

        let runtime_id = Uuid::new_v4();
        let name = tmux_name(Uuid::new_v4(), runtime_id);
        let tmux = root.join("tmux");
        let tmux_calls = root.join("tmux-calls");
        fixture_program(
            &tmux,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$4\" >> '{}'\ncase \"$4\" in\n  list-sessions) printf '%s\\n' '{name}' ;;\n  list-panes) printf '%s\\t%%1\\t41\\t0\\t100\\t30\\tcodex\\n' '{name}' ;;\n  *) exit 1 ;;\nesac\n",
                tmux_calls.display(),
            ),
        );
        let search = std::env::join_paths([Path::new("."), root]).unwrap();
        let fixture_catalog = catalog_dir.clone();
        let lane = CatalogLane::start(
            move || Ok(fixture_catalog),
            ServiceProfiles::new(Some(&search)),
        );
        let mut server = tokio::spawn(serve_local(root.to_owned(), tmux, Some(lane)));
        tokio::time::timeout(Duration::from_secs(2), async {
            while !root.join("agent.sock").exists() {
                if server.is_finished() {
                    panic!(
                        "fixture agent exited before opening its socket: {:?}",
                        (&mut server).await
                    );
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();

        let register = |locator: &Path| ProductRequest::RegisterWorkspace {
            locator: locator.to_str().unwrap().to_owned(),
        };
        let ProductResponse::WorkspaceRegistered { workspace: display } =
            product_request(root, &register(&selected)).await.unwrap()
        else {
            panic!("registration failed")
        };
        assert_eq!(display.label, "Вибрана папка");
        assert!(!display.workspace_id.is_nil());
        let check = |profile| ProductRequest::WorkspaceReadiness {
            workspace_id: display.workspace_id,
            profile,
        };
        assert_eq!(
            product_request(root, &register(&selected)).await.unwrap(),
            ProductResponse::WorkspaceRegistered {
                workspace: model::WorkspaceDisplay {
                    workspace_id: display.workspace_id,
                    label: display.label
                },
            }
        );
        for profile in [ProviderProfile::ClaudeCode, ProviderProfile::Codex] {
            assert_eq!(
                product_request(root, &check(profile)).await.unwrap(),
                ProductResponse::Ready {}
            );
        }
        let forged = ProductRequest::WorkspaceReadiness {
            workspace_id: Uuid::new_v4(),
            profile: ProviderProfile::Codex,
        };
        assert_eq!(
            product_request(root, &forged).await.unwrap(),
            ProductResponse::Blocked {
                issue: ProductIssue::WorkspaceUnknown
            }
        );
        let malformed = serde_json::json!({"product": { "op": "workspace_readiness", "workspace_id": display.workspace_id, "profile": "shell" }});
        let reply: ProductEnvelope<ProductResponse> = exchange(root, &malformed).await.unwrap();
        assert_eq!(
            reply.product,
            ProductResponse::Blocked {
                issue: ProductIssue::InvalidRequest
            }
        );
        let injected = serde_json::json!({"product": { "op": "workspace_readiness", "workspace_id": display.workspace_id,
            "profile": "codex", "argv": ["anything"] }});
        let reply: ProductEnvelope<ProductResponse> = exchange(root, &injected).await.unwrap();
        assert_eq!(
            reply.product,
            ProductResponse::Blocked {
                issue: ProductIssue::InvalidRequest
            }
        );

        fs::remove_file(root.join("node")).unwrap();
        assert_eq!(
            product_request(root, &check(ProviderProfile::Codex))
                .await
                .unwrap(),
            ProductResponse::Blocked {
                issue: ProductIssue::InterpreterUnavailable
            }
        );
        fs::set_permissions(root.join("claude"), fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            product_request(root, &check(ProviderProfile::ClaudeCode))
                .await
                .unwrap(),
            ProductResponse::Blocked {
                issue: ProductIssue::ProviderNotExecutable
            }
        );
        fs::remove_file(root.join("claude")).unwrap();
        assert_eq!(
            product_request(root, &check(ProviderProfile::ClaudeCode))
                .await
                .unwrap(),
            ProductResponse::Blocked {
                issue: ProductIssue::ProviderMissing
            }
        );
        if !geteuid().is_root() {
            assert_eq!(
                product_request(root, &register(&denied)).await.unwrap(),
                ProductResponse::Blocked {
                    issue: ProductIssue::FolderAccessDenied
                }
            );
            fs::set_permissions(&workspace, fs::Permissions::from_mode(0o000)).unwrap();
            assert_eq!(
                product_request(root, &check(ProviderProfile::Codex))
                    .await
                    .unwrap(),
                ProductResponse::Blocked {
                    issue: ProductIssue::FolderAccessDenied
                }
            );
            fs::set_permissions(&workspace, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::rename(&workspace, root.join("moved")).unwrap();
        fs::create_dir(&workspace).unwrap();
        assert_eq!(
            product_request(root, &check(ProviderProfile::Codex))
                .await
                .unwrap(),
            ProductResponse::Blocked {
                issue: ProductIssue::WorkspaceChanged
            }
        );
        fs::remove_file(&selected).unwrap();
        symlink(root.join("moved"), &selected).unwrap();
        assert_eq!(
            product_request(root, &check(ProviderProfile::Codex))
                .await
                .unwrap(),
            ProductResponse::Blocked {
                issue: ProductIssue::WorkspaceChanged
            }
        );
        let non_utf8 = root.join(std::ffi::OsString::from_vec(vec![0xff]));
        assert_eq!(
            product_launch::resolve_workspace(&non_utf8),
            Err(ProductIssue::UnsupportedPath)
        );
        assert!(!provider_runs.exists());
        assert!(!tmux_calls.exists());

        fs::rename(
            catalog_dir.join("catalog.sqlite3"),
            catalog_dir.join("retained.sqlite3"),
        )
        .unwrap();
        assert_eq!(
            product_request(root, &check(ProviderProfile::Codex))
                .await
                .unwrap(),
            ProductResponse::Blocked {
                issue: ProductIssue::CatalogMissing
            }
        );
        let Response::Runtimes { runtimes } = request(root, &Request::List {}).await.unwrap()
        else {
            panic!("inventory unavailable")
        };
        assert_eq!(runtimes[0].runtime_id, runtime_id);
        let Response::Attachment { tmux_name } = request(root, &Request::Attach { runtime_id })
            .await
            .unwrap()
        else {
            panic!("attachment unavailable")
        };
        assert_eq!(tmux_name, name);
        assert_eq!(
            fs::read_to_string(tmux_calls).unwrap(),
            "list-sessions\nlist-panes\nlist-sessions\nlist-panes\n"
        );
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}
