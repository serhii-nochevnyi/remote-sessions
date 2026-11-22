use crate::{
    catalog::{
        CatalogLane, Operation, OperationStage, PaneIdentity, ServerGeneration, StoredOutcome,
    },
    model::{
        CreateState, LaunchOutcome, PaneLiveness, ProductIssue, ProviderProfile, Request, Response,
        Runtime, RuntimeOrigin, parse_name, tmux_name,
    },
    product_launch::{LAUNCH_COMMAND, LaunchGuard, NONCE_ENV, read_setup_fault, workspace_locator},
};
use anyhow::{Context, Result, bail, ensure};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{io::AsyncWriteExt, process::Command};
use uuid::Uuid;

pub struct Engine {
    pub tmux: PathBuf,
    pub socket: PathBuf,
    pub ownership: crate::ownership::Ownership,
}

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

// These words are parsed by tmux inside its format-only condition. Escaped
// bytes are data even when the path contains tmux syntax or shell metacharacters.
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

impl Engine {
    async fn command(&self, args: &[&str]) -> Result<String> {
        // Never start tmux here: its lifetime belongs to a separate launchd job.
        let output = tokio::time::timeout(
            Duration::from_secs(3),
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
        // The whole test and token installation run on the same server.
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
        // -c is itself a tmux format. Escape every literal hash before quoting.
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
        &mut self,
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
        // The transaction must acknowledge its commit to this caller before a
        // tmux client can be started. A lost commit acknowledgement never sends.
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
                // A storage or verification fault leaves the tmux inventory available.
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

    async fn find(&self, id: Uuid) -> Result<Runtime> {
        self.list()
            .await?
            .into_iter()
            .find(|r| r.runtime_id == id)
            .context("managed runtime not found; it may have ended with the tmux service")
    }

    pub async fn handle(&mut self, request: Request) -> Result<Response> {
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
                self.ownership.revoke(runtime_id);
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
                ensure!(!self.find(runtime_id).await?.exited, "Runtime has exited");
                Ok(Response::Control {
                    token: self.ownership.acquire(runtime_id, Instant::now())?,
                })
            }
            Request::Renew { runtime_id, token } => {
                self.ownership.renew(runtime_id, token, Instant::now())?;
                Ok(Response::Accepted {})
            }
            Request::Release { runtime_id, token } => {
                self.ownership.release(runtime_id, token);
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
                let runtime = self.find(runtime_id).await?;
                ensure!(!runtime.exited, "Runtime has exited");
                self.ownership
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
                let runtime = self.find(runtime_id).await?;
                ensure!(
                    runtime.pane_id == expected_pane_id,
                    "runtime precondition failed"
                );
                let target = format!("={}", runtime.tmux_name);
                self.ownership.revoke(runtime_id);
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
