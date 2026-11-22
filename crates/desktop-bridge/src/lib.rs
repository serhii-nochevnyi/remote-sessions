//! Local desktop views with host-enforced exclusive input leases.
use anyhow::{Context, Result, bail, ensure};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
pub use remote_sessions_host::model::{ProductIssue, ProviderProfile, WorkspaceDisplay};
use remote_sessions_host::{
    model::{ProductRequest, ProductResponse, Request, Response, Runtime},
    product_launch::workspace_locator,
    product_request, request,
};
use serde::Serialize;
use std::{
    collections::HashMap,
    io::Read,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
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
}
#[derive(Serialize)]
pub struct Chunk {
    pub writable: bool,
    pub data: Vec<u8>,
    pub closed: bool,
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
    token: Option<Uuid>,
    sequence: u64,
    refresh_at: Instant,
}
struct View {
    runtime_id: Uuid,
    directory: PathBuf,
    control: Arc<tokio::sync::Mutex<Control>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    receiver: tokio::sync::Mutex<mpsc::Receiver<Vec<u8>>>,
    closed: AtomicBool,
}
impl View {
    fn close(&self) {
        if !self.closed.swap(true, Ordering::SeqCst) {
            let control = self.control.clone();
            let directory = self.directory.clone();
            let runtime_id = self.runtime_id;
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    let mut control = control.lock().await;
                    if let Some(token) = control.token.take() {
                        let _ = request(&directory, &Request::Release { runtime_id, token }).await;
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

pub struct DesktopBridge {
    generation: AtomicU64,
    directory: Option<PathBuf>,
    tmux: PathBuf,
    views: Mutex<HashMap<String, Arc<View>>>,
}
impl DesktopBridge {
    pub fn new(directory: Option<PathBuf>, tmux: PathBuf) -> Self {
        Self {
            generation: AtomicU64::new(0),
            directory,
            tmux,
            views: Mutex::new(HashMap::new()),
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
            ProductResponse::Ready {} => bail!("Unexpected workspace selection response"),
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
            ProductResponse::WorkspaceRegistered { .. } => bail!("Unexpected readiness response"),
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
        let generation = self.generation.load(Ordering::SeqCst);
        let id = Uuid::parse_str(runtime_id)?;
        let directory = self.directory()?;
        let inventory = self.inventory().await?;
        let runtime = inventory
            .runtimes
            .iter()
            .find(|r| r.runtime_id == id)
            .context("Runtime no longer exists")?;
        ensure!(!runtime.exited, "Runtime has exited");
        let Response::Attachment { tmux_name } =
            request(directory, &Request::Attach { runtime_id: id }).await?
        else {
            bail!("Attachment not authorized")
        };
        ensure!(tmux_name == runtime.tmux_name, "Runtime identity changed");
        ensure!(
            (2..=500).contains(&runtime.width) && (2..=200).contains(&runtime.height),
            "Unsupported terminal dimensions"
        );
        let mut views = self
            .views
            .lock()
            .map_err(|_| anyhow::anyhow!("View lock unavailable"))?;
        ensure!(
            generation == self.generation.load(Ordering::SeqCst),
            "View context changed"
        );
        ensure!(views.len() < 8, "At most 8 local views may be attached");
        let pair = native_pty_system().openpty(PtySize {
            rows: runtime.height,
            cols: runtime.width,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut command = CommandBuilder::new(&self.tmux);
        command.args(["-N", "-S"]);
        command.arg(directory.join("tmux.sock"));
        command.args([
            "attach-session",
            "-f",
            "ignore-size",
            "-E",
            "-t",
            &format!("={tmux_name}"),
        ]);
        command.env("TERM", "xterm-256color");
        command.env_remove("TMUX");
        let mut reader = pair.master.try_clone_reader()?;
        let mut child = pair.slave.spawn_command(command)?;
        let killer = child.clone_killer();
        drop(pair.slave);
        let (sender, receiver) = mpsc::channel(16);
        std::thread::spawn(move || {
            let mut bytes = [0_u8; 8192];
            loop {
                match reader.read(&mut bytes) {
                    Ok(0) | Err(_) => break,
                    Ok(size) => {
                        if sender.blocking_send(bytes[..size].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        let view_id = Uuid::new_v4().to_string();
        views.insert(
            view_id.clone(),
            Arc::new(View {
                runtime_id: id,
                directory: directory.clone(),
                control: Arc::new(tokio::sync::Mutex::new(Control {
                    token: None,
                    sequence: 0,
                    refresh_at: Instant::now(),
                })),
                killer: Mutex::new(killer),
                master: Mutex::new(Some(pair.master)),
                receiver: tokio::sync::Mutex::new(receiver),
                closed: AtomicBool::new(false),
            }),
        );
        Ok(Attachment {
            view_id,
            cols: runtime.width,
            rows: runtime.height,
        })
    }
    pub async fn read(&self, view_id: &str) -> Result<Chunk> {
        let view = self
            .views
            .lock()
            .map_err(|_| anyhow::anyhow!("View lock unavailable"))?
            .get(view_id)
            .cloned()
            .context("View detached")?;
        if view.closed.load(Ordering::SeqCst) {
            return Ok(Chunk {
                writable: false,
                data: vec![],
                closed: true,
            });
        }
        let writable = self.refresh_control(&view).await;
        let mut receiver = view.receiver.lock().await;
        match tokio::time::timeout(Duration::from_secs(1), receiver.recv()).await {
            Ok(Some(data)) => Ok(Chunk {
                writable,
                data,
                closed: false,
            }),
            Ok(None) => Ok(Chunk {
                writable: false,
                data: vec![],
                closed: true,
            }),
            Err(_) => Ok(Chunk {
                writable,
                data: vec![],
                closed: false,
            }),
        }
    }
    async fn refresh_control(&self, view: &View) -> bool {
        let mut control = view.control.lock().await;
        if view.closed.load(Ordering::SeqCst) {
            return false;
        }
        if Instant::now() < control.refresh_at {
            return control.token.is_some();
        }
        control.refresh_at = Instant::now() + Duration::from_secs(3);
        let message = match control.token {
            Some(token) => Request::Renew {
                runtime_id: view.runtime_id,
                token,
            },
            None => Request::Acquire {
                runtime_id: view.runtime_id,
            },
        };
        match request(&view.directory, &message).await {
            Ok(Response::Control { token }) => {
                control.token = Some(token);
                control.sequence = 0;
                true
            }
            Ok(Response::Accepted {}) => true,
            _ => {
                control.token = None;
                false
            }
        }
    }
    pub async fn input(&self, view_id: &str, data: Vec<u8>, paste: bool) -> Result<()> {
        ensure!(
            !data.is_empty() && data.len() <= 4096,
            "Input must contain 1..4096 bytes"
        );
        let view = self
            .views
            .lock()
            .map_err(|_| anyhow::anyhow!("View lock unavailable"))?
            .get(view_id)
            .cloned()
            .context("View detached")?;
        let mut control = view.control.lock().await;
        ensure!(!view.closed.load(Ordering::SeqCst), "View detached");
        let token = control.token.context("Керування ще не отримано")?;
        let sequence = control.sequence;
        control.sequence += 1;
        let result = request(
            &view.directory,
            &Request::Input {
                runtime_id: view.runtime_id,
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
        control.token = None;
        let _ = request(
            &view.directory,
            &Request::Release {
                runtime_id: view.runtime_id,
                token,
            },
        )
        .await;
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
}
