//! Local-only P0 runtime adapter. Network transport is absent; local input requires an exclusive lease.
pub mod engine;
pub mod model;
pub mod ownership;

use anyhow::{Context, Result, ensure};
use engine::Engine;
use model::{Request, Response};
use nix::unistd::geteuid;
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
    validate_directory(directory)?;
    let mut stream = UnixStream::connect(directory.join("agent.sock"))
        .await
        .context("local agent is unavailable")?;
    ensure!(
        stream.peer_cred()?.uid() == geteuid().as_raw(),
        "unexpected agent UID"
    );
    let mut bytes = serde_json::to_vec(request)?;
    bytes.push(b'\n');
    ensure!(bytes.len() <= REQUEST_LIMIT, "request too large");
    tokio::time::timeout(IO_TIMEOUT, stream.write_all(&bytes)).await??;
    let result = read_frame(&mut stream, RESPONSE_LIMIT).await?;
    serde_json::from_slice(&result).context("invalid agent response")
}

pub async fn serve(directory: PathBuf, tmux: PathBuf) -> Result<()> {
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
                if stream.peer_cred()?.uid() != geteuid().as_raw() { continue; }
                let Ok(permit) = permits.clone().try_acquire_owned() else { continue; };
                let engine = engine.clone();
                jobs.spawn(async move {
                    let _permit = permit;
                    let result: Result<Response> = async {
                        let bytes = read_frame(&mut stream, REQUEST_LIMIT).await?;
                        let request: Request = serde_json::from_slice(&bytes)?;
                        engine.lock().await.handle(request).await
                    }.await;
                    let response = result.unwrap_or_else(|error| Response::Error { message: error.to_string() });
                    if let Ok(mut bytes) = serde_json::to_vec(&response) {
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
}
