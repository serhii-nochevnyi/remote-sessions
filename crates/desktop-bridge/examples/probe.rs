//! Uses the same bridge as Tauri against an already running local lab.
use anyhow::{Context, Result, bail, ensure};
use remote_sessions_desktop_bridge::{
    Attachment, DesktopBridge, GeometryEvent, GridAcknowledgement, ResizeEpoch, ResizeResult,
    TerminalClosure, TerminalSize, TerminalStatus,
};
use remote_sessions_host::{
    decode_local_fixture, encode_local_observation,
    model::{Request, ResizeIntent, Response, Runtime, TerminalTarget, TerminalView},
    request, validate_directory,
};
#[cfg(debug_assertions)]
use remote_sessions_host::{request_resize_probe, serve_resize_fixture};
use serde::{Deserialize, Serialize};
#[cfg(debug_assertions)]
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::{
    fs,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;
fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let first = args
        .next()
        .context("state directory or resize/pressure subcommand")?;
    #[cfg(debug_assertions)]
    if first == "resize-fixture-capture" {
        return capture_fixture_command(args.collect());
    }
    run(first, args)
}

#[tokio::main]
async fn run(first: String, mut args: impl Iterator<Item = String>) -> Result<()> {
    if first == "resize" {
        return resize_probe(args.collect()).await;
    }
    if first == "pressure" {
        return pressure_probe(args.collect()).await;
    }
    legacy(first.into(), args.next().context("runtime id")?).await
}

async fn legacy(directory: PathBuf, runtime: String) -> Result<()> {
    let bridge = DesktopBridge::new(Some(directory), "/opt/homebrew/bin/tmux".into());
    for _ in 0..3 {
        let before = bridge
            .inventory()
            .await?
            .runtimes
            .into_iter()
            .find(|r| r.runtime_id.to_string() == runtime)
            .expect("runtime exists");
        let view = bridge.attach(&runtime).await?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut output = Vec::new();
        let mut incarnation = None;
        while Instant::now() < deadline {
            let chunk = bridge.read(&view.view_id).await?;
            assert!(!chunk.closed);
            assert!(chunk.data.len() <= 8192);
            output.extend(chunk.data);
            if chunk.writable && String::from_utf8_lossy(&output).contains("P0 runtime alive") {
                incarnation = chunk.paste_incarnation;
                break;
            }
        }
        assert!(String::from_utf8_lossy(&output).contains("P0 runtime alive"));
        let incarnation = incarnation.context("Native writer grant unavailable")?;
        bridge
            .input(
                &view.view_id,
                "Native bridge — Україна\r".as_bytes().to_vec(),
                &incarnation.to_string(),
                false,
            )
            .await?;
        bridge.detach(&view.view_id);
        assert!(bridge.read(&view.view_id).await.is_err());
        let after = bridge
            .inventory()
            .await?
            .runtimes
            .into_iter()
            .find(|r| r.runtime_id == before.runtime_id)
            .expect("runtime survives");
        assert_eq!(before.pane_pid, after.pane_pid);
    }
    println!(
        "PASS native bridge live output, detach and fresh reconnect preserve runtime (3 cycles)"
    );
    Ok(())
}

#[derive(Deserialize)]
struct DesktopLab {
    state_dir: PathBuf,
    tmux: PathBuf,
    targets: Vec<String>,
    runtimes: Vec<Runtime>,
}

struct Lab {
    directory: PathBuf,
    tmux: PathBuf,
    runtime: Uuid,
    agent: String,
}

#[derive(Default, Serialize)]
struct Report {
    source: String,
    engine_version: String,
    binaries: Vec<String>,
    commands: Vec<String>,
    witnessed: Vec<&'static str>,
    statuses: Vec<TerminalStatus>,
    grids: Vec<GeometryEvent>,
    agent_pids: Vec<u32>,
    fixture_cleaned_up: bool,
    failure: Option<String>,
}

const EXPECTED: &[&str] = &[
    "all_observers_passive",
    "owner_and_spectator_two_targets",
    "wrong_scope_and_sequence",
    "same_size_intents",
    "unknown_before_and_stale_guard",
    "unknown_after_and_recovery",
    "delayed_epoch_setter",
    "yielding_negative_control",
    "handoff_historical_propagation",
    "expiry_and_late_propagation",
    "aba_provider_transition",
    "agent_only_restart",
];

fn option(args: &[String], flag: &str) -> Result<String> {
    let mut matches = args.chunks_exact(2).filter(|pair| pair[0] == flag);
    let value = matches.next().context("Missing probe option")?[1].clone();
    ensure!(matches.next().is_none(), "Duplicate probe option");
    Ok(value)
}

async fn command(path: &Path, args: &[&str]) -> Result<String> {
    let output = tokio::time::timeout(
        Duration::from_secs(3),
        tokio::process::Command::new(path)
            .args(args)
            .stdin(Stdio::null())
            .env_remove("TMUX")
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    ensure!(
        output.status.success(),
        "Command failed: {} {}",
        path.display(),
        args.first().unwrap_or(&"")
    );
    Ok(String::from_utf8(output.stdout)?.trim_end().to_owned())
}

async fn resize_probe(args: Vec<String>) -> Result<()> {
    ensure!(
        args.len() == 8,
        "Expected --state-dir, --runtime-id, --tmux-bin and --report"
    );
    let directory = PathBuf::from(option(&args, "--state-dir")?);
    let tmux = PathBuf::from(option(&args, "--tmux-bin")?);
    let runtime = Uuid::parse_str(&option(&args, "--runtime-id")?)?;
    let path = PathBuf::from(option(&args, "--report")?);
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let manifest: DesktopLab =
        decode_local_fixture(&fs::read(repo.join(".cache/desktop-lab.json"))?)?;
    validate_directory(&directory)?;
    ensure!(
        cfg!(target_os = "macos")
            && directory == manifest.state_dir
            && tmux == manifest.tmux
            && directory == directory.canonicalize()?
            && directory.starts_with("/private/tmp")
            && directory
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("rs-p0-"))
            && manifest
                .runtimes
                .first()
                .is_some_and(|first| first.runtime_id == runtime)
            && manifest.targets.len() == 2,
        "Resize matrix requires the recorded disposable P0 desktop lab and its probe runtime"
    );
    let session = command(Path::new("/usr/bin/id"), &["-u"]).await?;
    let prefix = format!("gui/{session}/dev.remotesessions.p0.");
    let (tmux_target, agent) = (&manifest.targets[0], &manifest.targets[1]);
    let label = agent
        .strip_prefix(&prefix)
        .and_then(|tail| tail.strip_suffix(".agent"))
        .context("Unexpected agent service target")?;
    ensure!(
        label.len() == 12
            && label.bytes().all(|c| c.is_ascii_hexdigit())
            && *tmux_target == format!("{prefix}{label}.tmux"),
        "Unexpected disposable service labels"
    );
    let job = command(Path::new("/bin/launchctl"), &["print", agent]).await?;
    let service_binary = directory.join("host-agent");
    ensure!(
        job.contains(
            service_binary
                .to_str()
                .context("Invalid agent service path")?
        ),
        "Unexpected agent service binary"
    );
    let lab = Lab {
        directory,
        tmux,
        runtime,
        agent: agent.clone(),
    };
    let mut report = Report::default();
    let result = async {
        let root = repo.to_str().context("Invalid checkout path")?;
        report.source = command(
            Path::new("/usr/bin/git"),
            &["-C", root, "rev-parse", "HEAD"],
        )
        .await?;
        report.engine_version = command(&lab.tmux, &["-V"]).await?;
        for binary in [
            lab.tmux.clone(),
            lab.directory.join("host-agent"),
            std::env::current_exe()?,
        ] {
            report.binaries.push(
                command(
                    Path::new("/usr/bin/shasum"),
                    &["-a", "256", binary.to_str().context("Invalid binary path")?],
                )
                .await?,
            );
        }
        run_matrix(&lab, &mut report).await?;
        ensure!(
            EXPECTED.iter().all(|case| report.witnessed.contains(case)),
            "Required native witness absent"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if let Err(error) = &result {
        report.failure = Some(format!("{error:#}"));
    }
    fs::write(path, encode_local_observation(&report)? + "\n")?;
    result
}

impl Lab {
    fn bridge(&self, directory: &Path) -> DesktopBridge {
        DesktopBridge::new(Some(directory.to_owned()), self.tmux.clone())
    }

    async fn health(&self) -> Result<u32> {
        match request(&self.directory, &Request::Health {}).await? {
            Response::Health {
                agent_pid,
                remote_execution_enabled: false,
            } => Ok(agent_pid),
            _ => bail!("Expected local agent health"),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct FixtureScope {
    socket: PathBuf,
    target: TerminalTarget,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
enum CaptureKind {
    Resize,
    EpochSetter,
}

impl CaptureKind {
    fn for_mode(mode: &str) -> Result<Self> {
        match mode {
            "before" | "after" => Ok(Self::Resize),
            "before-setter" => Ok(Self::EpochSetter),
            _ => bail!("Unknown guarded command kind"),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct CaptureAttempt {
    nonce: Uuid,
    kind: CaptureKind,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct GuardedCapture {
    attempt: CaptureAttempt,
    scope: FixtureScope,
    argv: Vec<String>,
}

fn guard_tests(guard: &str, field: &str, value: &str) -> bool {
    guard.contains(&format!("#{{==:#{{{field}}},{value}}}"))
}

fn epoch_marker(epoch: ResizeEpoch) -> String {
    format!("{}:{}", epoch.agent, epoch.owner)
}

fn canonical_uuid(value: &str) -> bool {
    Uuid::parse_str(value).is_ok_and(|id| !id.is_nil() && id.to_string() == value)
}

impl GuardedCapture {
    fn validate(&self, attempt: CaptureAttempt, scope: &FixtureScope) -> Result<()> {
        ensure!(
            self.attempt == attempt && !attempt.nonce.is_nil() && self.scope == *scope,
            "Guarded capture did not match the current attempt and target"
        );
        let [
            no_start,
            socket_flag,
            socket,
            command,
            format_flag,
            target_flag,
            pane,
            guard,
            branch,
        ] = self.argv.as_slice()
        else {
            bail!("Unexpected captured tmux argv length");
        };
        let target = &scope.target;
        ensure!(
            [no_start, socket_flag, command, format_flag, target_flag].map(String::as_str)
                == ["-N", "-S", "if-shell", "-F", "-t"]
                && Path::new(socket) == scope.socket.as_path()
                && *pane == target.pane
                && guard.starts_with("#{&&:")
                && !guard.contains("#(")
                && guard_tests(
                    guard,
                    "@rs_create_generation",
                    &target.generation.to_string()
                )
                && guard_tests(guard, "session_id", &target.session)
                && guard_tests(guard, "window_id", &target.window)
                && guard_tests(guard, "pane_id", pane)
                && guard_tests(guard, "pane_pid", &target.pid.to_string()),
            "Captured guard did not match the fixture scope"
        );

        let nodes: Vec<_> = branch.split(" ; ").collect();
        let [mutation, marker, receipt] = nodes.as_slice() else {
            bail!("Unexpected captured tmux branch");
        };
        let mutation: Vec<_> = mutation.split(' ').collect();
        let marker: Vec<_> = marker.split(' ').collect();
        match attempt.kind {
            CaptureKind::Resize => {
                let ["resize-window", "-t", window, "-x", cols, "-y", rows] = mutation.as_slice()
                else {
                    bail!("Unexpected captured resize mutation");
                };
                let [
                    "set-option",
                    "-p",
                    "-t",
                    marker_pane,
                    "@rs_resize_intent",
                    intent,
                ] = marker.as_slice()
                else {
                    bail!("Unexpected captured resize marker");
                };
                let fields: Vec<_> = intent.split(':').collect();
                let [agent, owner, id, revision, intent_cols, intent_rows] = fields.as_slice()
                else {
                    bail!("Unexpected captured resize intent");
                };
                let size = TerminalSize {
                    cols: cols.parse()?,
                    rows: rows.parse()?,
                };
                ensure!(
                    *window == target.window.as_str()
                        && *marker_pane == pane.as_str()
                        && size.supported()
                        && *intent_cols == *cols
                        && *intent_rows == *rows
                        && [agent, owner, id]
                            .into_iter()
                            .all(|field| canonical_uuid(field))
                        && revision.parse::<u64>().is_ok_and(|revision| revision > 0)
                        && guard_tests(guard, "@rs_resize_epoch", &format!("{agent}:{owner}"))
                        && *receipt == "display-message -p rs-committed",
                    "Unexpected captured resize command"
                );
            }
            CaptureKind::EpochSetter => {
                let [
                    "set-option",
                    "-p",
                    "-t",
                    setter_pane,
                    "@rs_resize_epoch",
                    epoch,
                ] = mutation.as_slice()
                else {
                    bail!("Unexpected captured epoch mutation");
                };
                let [
                    "set-option",
                    "-p",
                    "-t",
                    marker_pane,
                    "@rs_paste_admission",
                    ready,
                ] = marker.as_slice()
                else {
                    bail!("Unexpected captured epoch marker");
                };
                ensure!(
                    *setter_pane == pane.as_str()
                        && *marker_pane == pane.as_str()
                        && epoch
                            .split_once(':')
                            .is_some_and(
                                |(agent, owner)| canonical_uuid(agent) && canonical_uuid(owner)
                            )
                        && ready.starts_with(&format!(
                            "v1~{}~{}~",
                            target.runtime_id, target.generation
                        ))
                        && ready
                            .rsplit_once("~r~")
                            .is_some_and(|(prefix, id)| prefix.ends_with(&format!("~{epoch}"))
                                && canonical_uuid(id))
                        && ready
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || b"~-:".contains(&byte))
                        && *receipt == format!("display-message -p -l -t {pane} rs-fenced"),
                    "Unexpected captured epoch setter"
                );
            }
        }
        Ok(())
    }
}

fn write_atomic(path: &Path, contents: impl AsRef<[u8]>) -> Result<()> {
    let incoming = path.with_extension(format!("{}.incoming", Uuid::new_v4().simple()));
    fs::write(&incoming, contents)?;
    fs::rename(incoming, path)?;
    Ok(())
}

#[cfg(debug_assertions)]
fn capture_fixture_command(args: Vec<String>) -> Result<()> {
    let [directory, mode, nonce, argv @ ..] = args.as_slice() else {
        bail!("Incomplete guarded capture");
    };
    let directory = Path::new(directory);
    validate_directory(directory)?;
    let scope: FixtureScope = decode_local_fixture(&fs::read(directory.join("scope"))?)?;
    let attempt = CaptureAttempt {
        nonce: Uuid::parse_str(nonce)?,
        kind: CaptureKind::for_mode(mode)?,
    };
    let capture = GuardedCapture {
        attempt,
        scope,
        argv: argv.to_vec(),
    };
    capture.validate(attempt, &capture.scope)?;
    write_atomic(
        &directory.join("captured"),
        encode_local_observation(&capture)?,
    )
}

struct FixtureAgent {
    directory: PathBuf,
    scope: FixtureScope,
    job: tokio::task::JoinHandle<Result<()>>,
}

impl FixtureAgent {
    #[cfg(debug_assertions)]
    async fn start(lab: &Lab, target: TerminalTarget) -> Result<Self> {
        let name = Uuid::new_v4().simple().to_string();
        let directory = lab.directory.join(format!("fixture-{}", &name[..8]));
        fs::DirBuilder::new().mode(0o700).create(&directory)?;
        let scope = FixtureScope {
            socket: lab.directory.join("tmux.sock"),
            target,
        };
        let configured = (|| {
            fs::write(
                directory.join("bin"),
                lab.tmux.as_os_str().as_encoded_bytes(),
            )?;
            fs::write(
                directory.join("probe"),
                std::env::current_exe()?.as_os_str().as_encoded_bytes(),
            )?;
            fs::write(directory.join("scope"), encode_local_observation(&scope)?)?;
            fs::write(directory.join("mode"), "\n")?;
            let wrapper = directory.join("tmux");
            fs::write(
                &wrapper,
                r#"#!/bin/sh
set -eu
root=$(dirname "$0")
tmux=$(cat "$root/bin")
mode=
nonce=
read -r mode nonce < "$root/mode" || true
case "${4:-}:$mode:${9:-}" in
    if-shell:before:resize-window*|if-shell:after:resize-window*|if-shell:before-setter:'set-option -p -t '*" @rs_resize_epoch "*)
        probe=$(cat "$root/probe")
        "$probe" resize-fixture-capture "$root" "$mode" "$nonce" "$@"
        if [ "$mode" = after ]; then
            "$tmux" "$@" > "$root/receipt-$nonce"
        fi
        exec /bin/sleep 2
        ;;
    display-message:delay-sample:*)
        case "$*" in *'@rs_resize_intent'*) sleep 0.28 ;; esac
        ;;
esac
exec "$tmux" "$@"
"#,
            )?;
            fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700))?;
            Ok::<_, anyhow::Error>(wrapper)
        })();
        let wrapper = match configured {
            Ok(wrapper) => wrapper,
            Err(error) => {
                let _ = fs::remove_dir_all(&directory);
                return Err(error);
            }
        };
        let job = tokio::spawn(serve_resize_fixture(
            directory.clone(),
            wrapper,
            scope.socket.clone(),
            scope.target.clone(),
        ));
        let mut fixture = Self {
            directory,
            scope,
            job,
        };
        tokio::time::timeout(Duration::from_secs(3), async {
            while !fixture.directory.join("agent.sock").exists() {
                if fixture.job.is_finished() {
                    (&mut fixture.job).await??;
                    bail!("Fixture agent exited during startup");
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        Ok(fixture)
    }

    #[cfg(not(debug_assertions))]
    async fn start(_lab: &Lab, _target: TerminalTarget) -> Result<Self> {
        bail!("The resize fixture requires a debug probe")
    }

    fn mode(&self, mode: &str) -> Result<()> {
        write_atomic(&self.directory.join("mode"), format!("{mode}\n"))
    }

    fn arm(&self, mode: &str) -> Result<CaptureAttempt> {
        let attempt = CaptureAttempt {
            nonce: Uuid::new_v4(),
            kind: CaptureKind::for_mode(mode)?,
        };
        match fs::remove_file(self.directory.join("captured")) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        self.mode(&format!("{mode} {}", attempt.nonce))?;
        Ok(attempt)
    }

    fn captured(&self, attempt: CaptureAttempt) -> Result<Vec<String>> {
        let capture: GuardedCapture =
            decode_local_fixture(&fs::read(self.directory.join("captured"))?)?;
        capture.validate(attempt, &self.scope)?;
        Ok(capture.argv)
    }

    async fn capture_delayed_setter(&self, owner: &Owner) -> Result<Vec<String>> {
        let attempt = self.arm("before-setter")?;
        let acquired = Owner::acquire(&self.directory, owner.view.clone()).await;
        self.mode("")?;
        ensure!(acquired.is_err(), "Unconfirmed setter granted a token");
        let argv = self.captured(attempt)?;
        ensure!(
            guard_tests(&argv[7], "@rs_resize_epoch", &epoch_marker(owner.epoch)),
            "Captured setter did not guard the predecessor epoch"
        );
        Ok(argv)
    }

    fn captured_resize(
        &self,
        attempt: CaptureAttempt,
        intent: ResizeIntent,
        size: TerminalSize,
    ) -> Result<Vec<String>> {
        let argv = self.captured(attempt)?;
        let marker = format!("{}:{}:", epoch_marker(intent.epoch), intent.id,);
        let branch = &argv[8];
        ensure!(
            branch.starts_with(&format!(
                "resize-window -t {} -x {} -y {} ; ",
                self.scope.target.window, size.cols, size.rows
            )) && branch.contains(&format!(" @rs_resize_intent {marker}")),
            "Captured resize did not match the admitted intent"
        );
        Ok(argv)
    }

    #[cfg(debug_assertions)]
    async fn expiry_attempt(
        &self,
        owner: &Owner,
        attempt: Uuid,
        size: TerminalSize,
    ) -> Result<Response> {
        request_resize_probe(
            &self.directory,
            &owner.resize_request(0, size),
            owner.epoch,
            attempt,
        )
        .await
    }

    #[cfg(not(debug_assertions))]
    async fn expiry_attempt(
        &self,
        _owner: &Owner,
        _attempt: Uuid,
        _size: TerminalSize,
    ) -> Result<Response> {
        bail!("The resize fixture requires a debug probe")
    }

    fn confirm_first_grant(&self, owner: &Owner, observed: &TerminalStatus) -> Result<()> {
        ensure!(
            observed.terminal == owner.view
                && observed.terminal.target == self.scope.target
                && observed.epoch == Some(owner.epoch)
                && owner.epoch.agent == owner.view.agent_epoch
                && !owner.epoch.owner.is_nil()
                && !owner.token.is_nil()
                && observed.provider.is_some(),
            "Exact first fixture grant and target observation are required"
        );
        Ok(())
    }

    async fn close(mut self) -> Result<()> {
        self.job.abort();
        let _ = (&mut self.job).await;
        fs::remove_dir_all(&self.directory)?;
        Ok(())
    }
}

impl Drop for FixtureAgent {
    fn drop(&mut self) {
        self.job.abort();
        let _ = fs::remove_dir_all(&self.directory);
    }
}

struct Owner {
    view: TerminalView,
    token: Uuid,
    epoch: ResizeEpoch,
}

impl Owner {
    async fn open(directory: &Path, runtime: Uuid) -> Result<TerminalView> {
        match request(
            directory,
            &Request::OpenTerminal {
                runtime_id: runtime,
                view_id: Uuid::new_v4(),
            },
        )
        .await?
        {
            Response::TerminalOpened { status } => Ok(status.terminal),
            _ => bail!("Native terminal unavailable"),
        }
    }

    async fn acquire(directory: &Path, view: TerminalView) -> Result<Self> {
        match request(
            directory,
            &Request::AcquireTerminal {
                terminal: view.clone(),
            },
        )
        .await?
        {
            Response::TerminalControl { token, epoch } => Ok(Self { view, token, epoch }),
            _ => bail!("Native owner acquisition was unconfirmed"),
        }
    }

    async fn release(&self, directory: &Path) -> Result<()> {
        let response = request(
            directory,
            &Request::Release {
                runtime_id: self.view.target.runtime_id,
                token: self.token,
            },
        )
        .await?;
        ensure!(matches!(response, Response::Accepted {}), "Release failed");
        Ok(())
    }

    async fn resize(
        &self,
        directory: &Path,
        sequence: u64,
        size: TerminalSize,
    ) -> Result<ResizeResult> {
        match request(directory, &self.resize_request(sequence, size)).await? {
            Response::Resize { resize } => Ok(resize),
            _ => bail!("Native resize admission rejected"),
        }
    }

    fn resize_request(&self, sequence: u64, size: TerminalSize) -> Request {
        Request::ResizeTerminal {
            terminal: self.view.clone(),
            token: self.token,
            sequence,
            size,
        }
    }

    async fn commit(
        &self,
        directory: &Path,
        sequence: u64,
        size: TerminalSize,
    ) -> Result<TerminalStatus> {
        match self.resize(directory, sequence, size).await? {
            ResizeResult::Committed { status, .. } => Ok(status),
            _ => bail!("Native target commit was not confirmed"),
        }
    }
}

async fn status(directory: &Path, terminal: &TerminalView) -> Result<TerminalStatus> {
    match request(
        directory,
        &Request::TerminalStatus {
            terminal: terminal.clone(),
        },
    )
    .await?
    {
        Response::TerminalStatus { status } if status.terminal == *terminal => Ok(status),
        _ => bail!("Native status unavailable"),
    }
}

async fn wait_provider(directory: &Path, committed: &TerminalStatus) -> Result<TerminalStatus> {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let observed = status(directory, &committed.terminal).await?;
            ensure!(
                observed.committed == committed.committed,
                "Committed target changed"
            );
            if observed
                .provider
                .is_some_and(|provider| provider.size == committed.committed.size)
            {
                return Ok(observed);
            }
            tokio::time::sleep(Duration::from_millis(4)).await;
        }
    })
    .await
    .context("Provider did not reach the committed size")?
}

async fn await_owner(bridge: &DesktopBridge, view: &Attachment) -> Result<ResizeEpoch> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let chunk = bridge.read(&view.view_id).await?;
            ensure!(
                !chunk.closed && chunk.geometry.is_none(),
                "View changed before owner acquisition"
            );
            if chunk.writable {
                return bridge
                    .status(&view.view_id)
                    .await?
                    .epoch
                    .context("Missing active epoch");
            }
        }
    })
    .await
    .context("Timed out acquiring native view")?
}

async fn await_grid(
    bridge: &DesktopBridge,
    view: &Attachment,
    committed: &TerminalStatus,
    owner: bool,
) -> Result<GeometryEvent> {
    let renderer_id = Uuid::new_v4();
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut applied = None;
        loop {
            let chunk = bridge.read(&view.view_id).await?;
            ensure!(
                !chunk.closed && chunk.writable == owner,
                "View closed or ownership changed"
            );
            if let Some(event) = chunk.geometry {
                ensure!(
                    event.status.terminal == view.status.terminal,
                    "Attachment identity changed"
                );
                let event = bridge
                    .acknowledge_grid(
                        &view.view_id,
                        GridAcknowledgement {
                            renderer_id,
                            epoch: event.status.epoch,
                            revision: event.status.committed.revision,
                            size: event.status.committed.size,
                            focused: false,
                        },
                    )
                    .await?;
                ensure!(
                    event.status.committed == committed.committed,
                    "Unexpected bridge revision"
                );
                applied = Some(event);
            }
            if let Some(event) = &mut applied {
                let observed = bridge.status(&view.view_id).await?;
                ensure!(
                    observed.committed == committed.committed,
                    "Unexpected native target"
                );
                if observed
                    .provider
                    .is_some_and(|sample| sample.size == committed.committed.size)
                {
                    event.status = observed;
                    return Ok(event.clone());
                }
            }
        }
    })
    .await
    .context("Bridge grids did not converge")?
}

const NORMAL: TerminalSize = TerminalSize {
    cols: 119,
    rows: 40,
};
const NARROW: TerminalSize = TerminalSize { cols: 77, rows: 26 };
const LATEST: TerminalSize = TerminalSize {
    cols: 106,
    rows: 35,
};

async fn positive(lab: &Lab, report: &mut Report) -> Result<()> {
    let bridge = lab.bridge(&lab.directory);
    let browser = lab.bridge(&lab.directory);
    let spectator = browser.attach_observer(&lab.runtime.to_string()).await?;
    let observer = bridge.attach_observer(&lab.runtime.to_string()).await?;
    let baseline = bridge.status(&observer.view_id).await?;
    for _ in 0..4 {
        ensure!(
            !bridge.read(&observer.view_id).await?.writable,
            "Observer acquired control"
        );
        ensure!(
            !browser.read(&spectator.view_id).await?.writable,
            "Spectator acquired control"
        );
        ensure!(
            browser.status(&spectator.view_id).await?.committed == baseline.committed,
            "Observer changed committed target"
        );
    }
    report.witnessed.push("all_observers_passive");
    bridge.detach(&observer.view_id);
    let owner = bridge.attach(&lab.runtime.to_string()).await?;
    let epoch = await_owner(&bridge, &owner).await?;
    ensure!(
        browser
            .resize(&spectator.view_id, NARROW, epoch)
            .await
            .is_err(),
        "Spectator admitted a resize"
    );
    let targets = if baseline.committed.size == NARROW {
        [NORMAL, NARROW]
    } else {
        [NARROW, NORMAL]
    };
    for size in targets {
        let ResizeResult::Committed { status, .. } =
            bridge.resize(&owner.view_id, size, epoch).await?
        else {
            bail!("Bridge owner resize was unconfirmed");
        };
        report.statuses.push(status.clone());
        let (left, right) = tokio::try_join!(
            await_grid(&bridge, &owner, &status, true),
            await_grid(&browser, &spectator, &status, false),
        )?;
        ensure!(
            left.status.terminal.target == right.status.terminal.target,
            "Views reached different runtime targets"
        );
        report.grids.extend([left, right]);
    }
    bridge.detach(&owner.view_id);
    browser.detach(&spectator.view_id);
    ensure!(
        bridge.resize(&owner.view_id, NARROW, epoch).await.is_err(),
        "Detached view admitted resize"
    );
    report.witnessed.push("owner_and_spectator_two_targets");
    Ok(())
}

async fn saved_command(lab: &Lab, args: &[String], report: &mut Report) -> Result<String> {
    report
        .commands
        .push(format!("{} {}", lab.tmux.display(), args.join(" ")));
    command(
        &lab.tmux,
        &args.iter().map(String::as_str).collect::<Vec<_>>(),
    )
    .await
}

async fn wrong_scope(directory: &Path, owner: &Owner, report: &mut Report) -> Result<()> {
    let initial = status(directory, &owner.view).await?;
    let mut runtime = owner.view.clone();
    runtime.target.runtime_id = Uuid::new_v4();
    let mut generation = owner.view.clone();
    generation.target.generation = Uuid::new_v4();
    let mut pane = owner.view.clone();
    pane.target.pane = "%4294967295".into();
    let mut view = owner.view.clone();
    view.view_id = Uuid::new_v4();
    let mut agent = owner.view.clone();
    agent.agent_epoch = Uuid::new_v4();
    for (terminal, sequence, size) in [
        (runtime, 0, NORMAL),
        (generation, 0, NORMAL),
        (pane, 0, NORMAL),
        (view, 0, NORMAL),
        (agent, 0, NORMAL),
        (owner.view.clone(), 1, NORMAL),
        (
            owner.view.clone(),
            0,
            TerminalSize {
                cols: 501,
                ..NORMAL
            },
        ),
    ] {
        let reply = request(
            directory,
            &Request::ResizeTerminal {
                terminal,
                sequence,
                token: owner.token,
                size,
            },
        )
        .await?;
        ensure!(
            matches!(reply, Response::Error { .. }),
            "Invalid scope, sequence or bounds admitted"
        );
    }
    ensure!(
        status(directory, &owner.view).await?.committed == initial.committed,
        "Wrong-scope request changed target"
    );
    report.witnessed.push("wrong_scope_and_sequence");
    let same = owner.commit(directory, 0, initial.committed.size).await?;
    ensure!(
        same.committed.revision == initial.committed.revision + 1
            && same.committed.intent != initial.committed.intent,
        "Same-size request lacked a distinct committed intent"
    );
    ensure!(
        owner.resize(directory, 0, NORMAL).await.is_err(),
        "Replayed resize sequence admitted"
    );
    report.statuses.push(same);
    report.witnessed.push("same_size_intents");
    Ok(())
}

async fn unknown_and_fences(
    lab: &Lab,
    fixture: &FixtureAgent,
    report: &mut Report,
    first: Owner,
) -> Result<Owner> {
    let dir = &fixture.directory;
    let attempt = fixture.arm("before")?;
    let ResizeResult::Uncertain { intent, .. } = first.resize(dir, 1, NARROW).await? else {
        bail!("Lost-before request failed to close admission");
    };
    let delayed = fixture.captured_resize(attempt, intent, NARROW)?;
    let unchanged = status(dir, &first.view).await?;
    ensure!(
        unchanged.committed.intent != Some(intent),
        "Lost-before request committed"
    );
    ensure!(
        first.resize(dir, 2, NORMAL).await.is_err(),
        "Unknown outcome reopened old lease"
    );
    fixture.mode("")?;
    let second = Owner::acquire(dir, first.view.clone()).await?;
    ensure!(
        second.epoch != first.epoch && saved_command(lab, &delayed, report).await?.is_empty(),
        "Old guard committed after successor"
    );
    ensure!(
        status(dir, &second.view).await?.committed == unchanged.committed,
        "Delayed guard changed target"
    );
    report.statuses.push(unchanged);
    report.witnessed.push("unknown_before_and_stale_guard");

    let attempt = fixture.arm("after")?;
    let ResizeResult::Uncertain { intent, .. } = second.resize(dir, 0, LATEST).await? else {
        bail!("Lost-after request failed to close admission");
    };
    fixture.captured_resize(attempt, intent, LATEST)?;
    ensure!(
        fs::read_to_string(dir.join(format!("receipt-{}", attempt.nonce)))?.trim()
            == "rs-committed",
        "Lost-after target was never committed"
    );
    let committed = status(dir, &second.view).await?;
    ensure!(
        committed.committed.intent == Some(intent),
        "Lost-after intent absent"
    );
    ensure!(
        second.resize(dir, 1, NORMAL).await.is_err(),
        "Lost-after predecessor still admitted writes"
    );

    let setter = fixture.capture_delayed_setter(&second).await?;
    let third = Owner::acquire(dir, second.view).await?;
    let fresh = status(dir, &third.view).await?;
    ensure!(
        fresh.epoch == Some(third.epoch)
            && fresh.committed == committed.committed
            && fresh.provider.is_some(),
        "Recovery failed to bracket the committed target with a provider sample"
    );
    ensure!(
        saved_command(lab, &setter, report).await?.is_empty(),
        "Obsolete setter overwrote authority"
    );
    ensure!(
        status(dir, &third.view).await?.epoch == Some(third.epoch),
        "Installed owner changed after stale setter"
    );
    report.statuses.extend([committed, fresh]);
    report
        .witnessed
        .extend(["unknown_after_and_recovery", "delayed_epoch_setter"]);
    Ok(third)
}

async fn yielding_negative(
    lab: &Lab,
    fixture: &FixtureAgent,
    predecessor: Owner,
    report: &mut Report,
) -> Result<Owner> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let dir = &fixture.directory;
    let attempt = fixture.arm("before")?;
    let ResizeResult::Uncertain { intent, .. } = predecessor.resize(dir, 0, NARROW).await? else {
        bail!("Expected a captured predecessor command");
    };
    let mut delayed = fixture.captured_resize(attempt, intent, NARROW)?;
    fixture.mode("")?;
    let name = format!("rs-test-{}", Uuid::new_v4().simple());
    let socket = lab.directory.join("tmux.sock");
    let socket = socket.to_str().context("Invalid socket path")?;
    let prefix = ["-N", "-S", socket, "wait-for"];
    command(&lab.tmux, &[&prefix[..], &["-L", &name]].concat()).await?;

    delayed[8] = format!(
        "display-message -p rs-yielding ; wait-for -L {name} ; {} ; wait-for -U {name}",
        delayed[8]
    );
    report
        .commands
        .push(format!("{} {}", lab.tmux.display(), delayed.join(" ")));
    let witnessed = async {
        let mut child = tokio::process::Command::new(&lab.tmux)
            .args(&delayed)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let mut outputs = BufReader::new(
            child
                .stdout
                .take()
                .context("Missing negative control stdout")?,
        );
        let mut ready = String::new();
        tokio::time::timeout(Duration::from_secs(2), outputs.read_line(&mut ready)).await??;
        ensure!(
            ready.trim() == "rs-yielding",
            "Deliberate yield never passed predecessor guard"
        );
        let successor = Owner::acquire(dir, predecessor.view.clone()).await?;
        ensure!(
            successor.epoch != predecessor.epoch,
            "Negative control did not establish a successor"
        );
        command(&lab.tmux, &[&prefix[..], &["-U", &name]].concat()).await?;
        let mut committed = String::new();
        tokio::time::timeout(Duration::from_secs(2), outputs.read_line(&mut committed)).await??;
        ensure!(
            committed.trim() == "rs-committed",
            "Deliberately yielding guard could not resume"
        );
        ensure!(
            tokio::time::timeout(Duration::from_secs(2), child.wait())
                .await??
                .success(),
            "Negative fixture failed"
        );
        let observed = status(dir, &successor.view).await?;
        ensure!(
            observed.committed.intent == Some(intent) && observed.epoch == Some(successor.epoch),
            "Negative control lacked a late old-epoch commit"
        );
        report.statuses.push(observed);
        report.witnessed.push("yielding_negative_control");
        Ok(successor)
    }
    .await;
    if witnessed.is_err() {
        let _ = command(&lab.tmux, &[&prefix[..], &["-U", &name]].concat()).await;
    }
    witnessed
}

async fn send_probe_line(directory: &Path, owner: &Owner, line: &str) -> Result<()> {
    let reply = request(
        directory,
        &Request::Input {
            runtime_id: owner.view.target.runtime_id,
            token: owner.token,
            sequence: 0,
            data: format!("{line}\r").into_bytes(),
            paste: false,
        },
    )
    .await?;
    ensure!(
        matches!(reply, Response::Accepted {}),
        "Independent input sequence rejected"
    );
    Ok(())
}

async fn handoff(
    lab: &Lab,
    fixture: &FixtureAgent,
    old: Owner,
    report: &mut Report,
) -> Result<Owner> {
    let dir = &fixture.directory;
    let before_line = format!("owner-{}", Uuid::new_v4().simple());
    let after_line = format!("successor-{}", Uuid::new_v4().simple());
    send_probe_line(dir, &old, &before_line).await?;
    let prime = old.commit(dir, 0, NORMAL).await?;
    report.statuses.push(wait_provider(dir, &prime).await?);
    let historic = old.commit(dir, 1, NARROW).await?;
    old.release(dir).await?;
    ensure!(
        old.resize(dir, 2, NORMAL).await.is_err(),
        "Released view admitted a resize"
    );
    let successor = Owner::acquire(dir, old.view).await?;
    let still_pending = status(dir, &successor.view).await?;
    ensure!(
        still_pending.epoch == Some(successor.epoch)
            && still_pending.committed == historic.committed
            && still_pending
                .provider
                .is_some_and(|sample| sample.size == NORMAL),
        "No provider lag survived the successor acknowledgement"
    );
    let landed = wait_provider(dir, &historic).await?;
    ensure!(
        landed.epoch == Some(successor.epoch),
        "Historical observation lacked current successor"
    );
    let newest = successor.commit(dir, 0, LATEST).await?;
    send_probe_line(dir, &successor, &after_line).await?;
    report.statuses.extend([
        historic,
        still_pending,
        landed,
        wait_provider(dir, &newest).await?,
    ]);
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(output) = fs::read_to_string(lab.directory.join("input.txt"))
                && output.lines().filter(|line| *line == before_line).count() == 1
                && output.lines().filter(|line| *line == after_line).count() == 1
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("Expected exactly one line per independent input sequence")?;
    report.witnessed.push("handoff_historical_propagation");
    Ok(successor)
}

async fn aba(fixture: &FixtureAgent, owner: &Owner, report: &mut Report) -> Result<()> {
    let dir = &fixture.directory;
    let prime = owner.commit(dir, 1, NORMAL).await?;
    report.statuses.push(wait_provider(dir, &prime).await?);
    let middle = owner.commit(dir, 2, NARROW).await?;
    let latest = owner.commit(dir, 3, NORMAL).await?;
    ensure!(
        latest.committed.revision == middle.committed.revision + 1
            && latest.committed.intent != middle.committed.intent,
        "ABA intent identities collapsed"
    );
    report.statuses.extend([middle, latest.clone()]);
    let observations = tokio::time::timeout(Duration::from_secs(3), async {
        let mut states = Vec::new();
        for wanted in [NORMAL, NARROW, NORMAL] {
            loop {
                let sampled = status(dir, &owner.view).await?;
                ensure!(
                    sampled.committed == latest.committed,
                    "Committed ABA target changed"
                );
                if sampled.provider.is_some_and(|sample| sample.size == wanted) {
                    states.push(sampled);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
        Ok::<_, anyhow::Error>(states)
    })
    .await
    .context("Missing equal sample followed by historical and final ABA observations")??;
    report.statuses.extend(observations);
    report.witnessed.push("aba_provider_transition");
    Ok(())
}

async fn expiry(fixture: &FixtureAgent, terminal: TerminalView, report: &mut Report) -> Result<()> {
    let dir = &fixture.directory;
    let owner = Owner::acquire(dir, terminal.clone()).await?;
    let granted = tokio::time::Instant::now();
    let baseline = owner.commit(dir, 0, NORMAL).await?;
    report.statuses.push(wait_provider(dir, &baseline).await?);
    tokio::time::sleep_until(granted + Duration::from_millis(14_830)).await;
    let prime = owner.commit(dir, 1, LATEST).await?;
    report.statuses.push(wait_provider(dir, &prime).await?);
    let historic = owner.commit(dir, 2, NARROW).await?;
    tokio::time::sleep_until(granted + Duration::from_millis(15_005)).await;
    let late = status(dir, &terminal).await?;
    ensure!(
        late.committed == historic.committed
            && late.provider.is_some_and(|sample| sample.size == LATEST),
        "No admitted effect was still pending after expiry"
    );
    ensure!(
        owner.resize(dir, 3, NORMAL).await.is_err(),
        "Expired owner admitted a new resize"
    );
    report
        .statuses
        .extend([historic.clone(), late, wait_provider(dir, &historic).await?]);

    let next = Owner::acquire(dir, terminal).await?;
    let granted = tokio::time::Instant::now();
    let before = status(dir, &next.view).await?;
    let attempt = Uuid::new_v4();
    fixture.mode("delay-sample")?;
    tokio::time::sleep_until(granted + Duration::from_millis(14_850)).await;
    let reply = fixture.expiry_attempt(&next, attempt, NORMAL).await;
    fixture.mode("")?;
    let after = status(dir, &next.view).await?;
    confirm_expiry(attempt, reply, &next, NORMAL, &before, &after)?;
    report.statuses.extend([before, after]);
    report.witnessed.push("expiry_and_late_propagation");
    Ok(())
}

fn confirm_expiry(
    attempt: Uuid,
    reply: Result<Response>,
    owner: &Owner,
    proposed: TerminalSize,
    before: &TerminalStatus,
    after: &TerminalStatus,
) -> Result<()> {
    ensure!(
        matches!(reply?, Response::Error { message } if message == format!("resize admission expired: {attempt}")),
        "Exact resize expiry at host admission was unconfirmed"
    );
    ensure!(
        before.terminal == owner.view
            && after.terminal == owner.view
            && before.epoch == Some(owner.epoch)
            && after.epoch == Some(owner.epoch)
            && proposed != before.committed.size
            && before.committed == after.committed
            && before
                .provider
                .zip(after.provider)
                .is_some_and(|(before, after)| before.size == after.size),
        "Expired resize lacked unchanged geometry on the same target"
    );
    Ok(())
}

async fn run_matrix(lab: &Lab, report: &mut Report) -> Result<()> {
    report.agent_pids.push(lab.health().await?);
    let target = Owner::open(&lab.directory, lab.runtime).await?.target;
    let fixture = FixtureAgent::start(lab, target).await?;
    let dir = &fixture.directory;
    let terminal = Owner::open(dir, lab.runtime).await?;
    let first = Owner::acquire(dir, terminal.clone()).await?;
    let observed = status(dir, &terminal).await?;
    fixture.confirm_first_grant(&first, &observed)?;
    report.statuses.push(observed);
    wrong_scope(dir, &first, report).await?;
    let owner = unknown_and_fences(lab, &fixture, report, first).await?;
    let owner = yielding_negative(lab, &fixture, owner, report).await?;
    let owner = handoff(lab, &fixture, owner, report).await?;
    aba(&fixture, &owner, report).await?;
    owner.release(dir).await?;
    expiry(&fixture, terminal.clone(), report).await?;

    let last = Owner::acquire(dir, terminal).await?;
    let persisted = status(dir, &last.view).await?;
    let attempt = fixture.arm("before")?;
    let ResizeResult::Uncertain { intent, .. } = last.resize(dir, 0, LATEST).await? else {
        bail!("Restart fixture did not capture the old resize");
    };
    let delayed = fixture.captured_resize(attempt, intent, LATEST)?;
    let setter = fixture.capture_delayed_setter(&last).await?;
    fixture.close().await?;
    report.fixture_cleaned_up = true;

    report
        .commands
        .push(format!("launchctl kickstart -k {}", lab.agent));
    command(
        Path::new("/bin/launchctl"),
        &["kickstart", "-k", &lab.agent],
    )
    .await?;
    let old_pid = *report
        .agent_pids
        .first()
        .context("Missing original agent health")?;
    let pid = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if let Ok(pid) = lab.health().await
                && pid != old_pid
            {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .context("Agent did not restart")?;
    report.agent_pids.push(pid);

    let terminal = Owner::open(&lab.directory, lab.runtime).await?;
    ensure!(
        terminal.target == persisted.terminal.target,
        "Server, runtime or process changed across agent restart"
    );
    let fresh = Owner::acquire(&lab.directory, terminal).await?;
    let read = status(&lab.directory, &fresh.view).await?;
    ensure!(
        read.epoch == Some(fresh.epoch)
            && read.committed == persisted.committed
            && read.provider.is_some()
            && fresh.epoch.agent != last.epoch.agent,
        "Restart failed to retire the actual persisted predecessor"
    );
    ensure!(
        saved_command(lab, &delayed, report).await?.is_empty(),
        "Old resize committed after restart"
    );
    ensure!(
        saved_command(lab, &setter, report).await?.is_empty(),
        "Old epoch setter committed after restart"
    );
    ensure!(
        last.resize(&lab.directory, 0, NORMAL).await.is_err(),
        "Old agent scope admitted a new request"
    );
    let newest = fresh.commit(&lab.directory, 0, LATEST).await?;
    report.statuses.extend([
        persisted,
        read,
        wait_provider(&lab.directory, &newest).await?,
    ]);
    fresh.release(&lab.directory).await?;
    report.witnessed.push("agent_only_restart");
    positive(lab, report).await
}

#[cfg(all(test, debug_assertions))]
mod resize_probe_tests {
    use super::*;
    use remote_sessions_host::model::{CommittedGeometry, ObservationState, ProviderSample};
    use std::os::unix::fs::symlink;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::UnixListener,
    };

    fn fixture() -> (FixtureAgent, Owner, TerminalStatus) {
        let id = Uuid::new_v4().simple().to_string();
        let directory = Path::new("/tmp").join(format!("rs-rz-{}", &id[..12]));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .unwrap();
        let epoch = ResizeEpoch {
            agent: Uuid::new_v4(),
            owner: Uuid::new_v4(),
        };
        let view = TerminalView {
            target: TerminalTarget {
                runtime_id: Uuid::new_v4(),
                generation: Uuid::new_v4(),
                session: "$1".into(),
                window: "@2".into(),
                pane: "%3".into(),
                pid: 4,
                process_start: 5,
            },
            view_id: Uuid::new_v4(),
            agent_epoch: epoch.agent,
        };
        let status = TerminalStatus {
            terminal: view.clone(),
            epoch: Some(epoch),
            committed: CommittedGeometry {
                size: NARROW,
                revision: 0,
                intent: None,
            },
            provider: Some(ProviderSample {
                size: NARROW,
                sampled_at_us: 1,
            }),
            observation: ObservationState::Observed,
        };
        let socket_dir = directory.join("server");
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&socket_dir)
            .unwrap();
        let scope = FixtureScope {
            socket: socket_dir.join("tmux.sock"),
            target: view.target.clone(),
        };
        fs::write(
            directory.join("scope"),
            encode_local_observation(&scope).unwrap(),
        )
        .unwrap();
        let agent = FixtureAgent {
            directory,
            scope,
            job: tokio::spawn(std::future::pending()),
        };
        (
            agent,
            Owner {
                view,
                token: Uuid::new_v4(),
                epoch,
            },
            status,
        )
    }

    #[tokio::test]
    async fn w1_resize_probe_private_socket() {
        let (fixture, owner, observed) = fixture();
        let _socket = UnixListener::bind(&fixture.scope.socket).unwrap();
        let alias_dir = fixture.directory.join("alias");
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&alias_dir)
            .unwrap();
        let alias = alias_dir.join("tmux.sock");
        symlink(&fixture.scope.socket, &alias).unwrap();
        let tmux = fixture.directory.join("fake-tmux");
        fs::write(&tmux, "#!/bin/sh\n[ \"$3\" = \"$(dirname \"$0\")/server/tmux.sock\" ] && [ -S \"$3\" ] && echo real-socket >&2\nexit 1\n").unwrap();
        fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();
        for (path, expected) in [
            (alias, "private real tmux socket"),
            (fixture.scope.socket.clone(), "real-socket"),
        ] {
            let error = serve_resize_fixture(
                fixture.directory.clone(),
                tmux.clone(),
                path,
                owner.view.target.clone(),
            )
            .await
            .unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
            assert!(!fixture.directory.join("agent.sock").exists());
        }

        fixture.confirm_first_grant(&owner, &observed).unwrap();
        let mut wrong = observed.clone();
        wrong.terminal.target.process_start += 1;
        assert!(fixture.confirm_first_grant(&owner, &wrong).is_err());
        let mut wrong = observed.clone();
        wrong.provider = None;
        assert!(fixture.confirm_first_grant(&owner, &wrong).is_err());
        let mut wrong = observed.clone();
        wrong.epoch = None;
        assert!(fixture.confirm_first_grant(&owner, &wrong).is_err());
    }

    fn capture_args(fixture: &FixtureAgent, owner: &Owner, mode: &str, nonce: Uuid) -> Vec<String> {
        let target = &owner.view.target;
        let epoch = epoch_marker(owner.epoch);
        let guard = [
            format!("#{{==:#{{@rs_create_generation}},{}}}", target.generation),
            format!("#{{==:#{{session_id}},{}}}", target.session),
            format!("#{{==:#{{window_id}},{}}}", target.window),
            format!("#{{==:#{{pane_id}},{}}}", target.pane),
            format!("#{{==:#{{pane_pid}},{}}}", target.pid),
            format!("#{{==:#{{@rs_resize_epoch}},{epoch}}}"),
        ]
        .into_iter()
        .reduce(|left, right| format!("#{{&&:{left},{right}}}"))
        .unwrap();
        let pane = &target.pane;
        let branch = if mode == "before-setter" {
            let next = epoch_marker(ResizeEpoch {
                owner: Uuid::new_v4(),
                ..owner.epoch
            });
            format!(
                "set-option -p -t {pane} @rs_resize_epoch {next} ; set-option -p -t {pane} @rs_paste_admission v1~{}~{}~{next}~r~{} ; display-message -p -l -t {pane} rs-fenced",
                target.runtime_id,
                target.generation,
                Uuid::new_v4(),
            )
        } else {
            format!(
                "resize-window -t {} -x 119 -y 40 ; set-option -p -t {pane} @rs_resize_intent {epoch}:{}:1:119:40 ; display-message -p rs-committed",
                target.window,
                Uuid::new_v4(),
            )
        };
        vec![
            fixture.directory.to_str().unwrap().into(),
            mode.into(),
            nonce.to_string(),
            "-N".into(),
            "-S".into(),
            fixture.scope.socket.to_str().unwrap().into(),
            "if-shell".into(),
            "-F".into(),
            "-t".into(),
            pane.clone(),
            guard,
            branch,
        ]
    }

    #[tokio::test]
    async fn w1_resize_probe_attempt_capture() {
        for leg in ["handoff", "restart"] {
            for outcome in [
                "no dispatch",
                "stale resize",
                "wrong kind",
                "wrong nonce",
                "wrong argv",
                "current setter",
            ] {
                let (fixture, owner, _) = fixture();
                let predecessor = fixture.arm("before").unwrap();
                capture_fixture_command(capture_args(
                    &fixture,
                    &owner,
                    "before",
                    predecessor.nonce,
                ))
                .unwrap();
                let path = fixture.directory.join("captured");
                let stale = fs::read(&path).unwrap();
                assert!(fixture.captured(predecessor).is_ok());
                let listener = UnixListener::bind(fixture.directory.join("agent.sock")).unwrap();
                let fail_acquisition = async {
                    let (mut connection, _) = listener.accept().await.unwrap();
                    let mut frame = String::new();
                    BufReader::new(&mut connection)
                        .read_line(&mut frame)
                        .await
                        .unwrap();
                    let request: Request = decode_local_fixture(frame.as_bytes()).unwrap();
                    assert!(
                        matches!(request, Request::AcquireTerminal { terminal } if terminal == owner.view)
                    );
                    assert!(!path.exists(), "{leg} retained predecessor capture");
                    let control = fs::read_to_string(fixture.directory.join("mode")).unwrap();
                    let (mode, nonce) = control.trim().split_once(' ').unwrap();
                    assert_eq!(mode, "before-setter");
                    let nonce = Uuid::parse_str(nonce).unwrap();
                    match outcome {
                        "no dispatch" => {}
                        "stale resize" => write_atomic(&path, &stale).unwrap(),
                        "wrong kind" => {
                            capture_fixture_command(capture_args(&fixture, &owner, "before", nonce))
                                .unwrap()
                        }
                        "wrong nonce" => capture_fixture_command(capture_args(
                            &fixture,
                            &owner,
                            mode,
                            predecessor.nonce,
                        ))
                        .unwrap(),
                        "wrong argv" => {
                            let mut args = capture_args(&fixture, &owner, mode, nonce);
                            args[9] = "%9".into();
                            assert!(capture_fixture_command(args).is_err());
                        }
                        "current setter" => {
                            capture_fixture_command(capture_args(&fixture, &owner, mode, nonce))
                                .unwrap()
                        }
                        _ => unreachable!(),
                    }
                    let error = Response::Error {
                        message: "Acquisition unconfirmed".into(),
                    };
                    let line = encode_local_observation(&error).unwrap() + "\n";
                    connection.write_all(line.as_bytes()).await.unwrap();
                };
                let (capture, ()) = tokio::time::timeout(Duration::from_secs(5), async {
                    tokio::join!(fixture.capture_delayed_setter(&owner), fail_acquisition)
                })
                .await
                .unwrap();
                assert_eq!(
                    capture.is_ok(),
                    outcome == "current setter",
                    "{leg}: {outcome}: {capture:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn w1_resize_probe_expiry_witness() {
        let (_fixture, owner, before) = fixture();
        let attempt = Uuid::new_v4();
        let expired = |nonce| Response::Error {
            message: format!("resize admission expired: {nonce}"),
        };
        for failure in [
            Err(anyhow::anyhow!("transport lost")),
            Ok(Response::Error {
                message: "Resize authority changed".into(),
            }),
            Ok(Response::Error {
                message: "Resize sequence rejected".into(),
            }),
            Ok(expired(Uuid::new_v4())),
        ] {
            assert!(confirm_expiry(attempt, failure, &owner, NORMAL, &before, &before).is_err());
        }
        let mut after = before.clone();
        after.terminal.target.generation = Uuid::new_v4();
        assert!(
            confirm_expiry(
                attempt,
                Ok(expired(attempt)),
                &owner,
                NORMAL,
                &before,
                &after
            )
            .is_err()
        );
        let mut after = before.clone();
        after.epoch = None;
        assert!(
            confirm_expiry(
                attempt,
                Ok(expired(attempt)),
                &owner,
                NORMAL,
                &before,
                &after
            )
            .is_err()
        );
        let mut after = before.clone();
        after.committed.revision += 1;
        assert!(
            confirm_expiry(
                attempt,
                Ok(expired(attempt)),
                &owner,
                NORMAL,
                &before,
                &after
            )
            .is_err()
        );
        let mut after = before.clone();
        after.provider = None;
        assert!(
            confirm_expiry(
                attempt,
                Ok(expired(attempt)),
                &owner,
                NORMAL,
                &before,
                &after
            )
            .is_err()
        );
        after.provider = Some(ProviderSample {
            size: NARROW,
            sampled_at_us: 2,
        });
        confirm_expiry(
            attempt,
            Ok(expired(attempt)),
            &owner,
            NORMAL,
            &before,
            &after,
        )
        .unwrap();
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PressureHeartbeat {
    pid: u32,
    instance: Uuid,
    sequence: u64,
    at_ns: u64,
}

#[derive(Serialize)]
struct PressureView {
    view_id: String,
    pid: u32,
    started: String,
    rss_kib: u64,
}

impl PressureView {
    async fn observe(bridge: &DesktopBridge, view: &Attachment) -> Result<Self> {
        let pid = bridge.attachment_pid(&view.view_id)?;
        let pid_arg = pid.to_string();
        let ps = Path::new("/bin/ps");
        let started = command(ps, &["-p", &pid_arg, "-o", "lstart="]).await?;
        let rss = command(ps, &["-p", &pid_arg, "-o", "rss="]).await?;
        ensure!(
            !started.is_empty() && started.len() <= 32,
            "Attachment start unavailable"
        );
        Ok(Self {
            view_id: view.view_id.clone(),
            pid,
            started: started.trim().to_owned(),
            rss_kib: rss.trim().parse()?,
        })
    }

    async fn wait_reaped(&self) -> Result<()> {
        let pid = self.pid.to_string();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let current = tokio::process::Command::new("/bin/ps")
                    .args(["-p", &pid, "-o", "lstart="])
                    .kill_on_drop(true)
                    .output()
                    .await?;
                if !current.status.success()
                    || String::from_utf8_lossy(&current.stdout).trim() != self.started.trim()
                {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
        })
        .await
        .context("Disposable attachment client did not exit")?
    }
}

#[derive(Serialize)]
struct PressureOutput {
    bytes: usize,
    first_at_us: u64,
    last_at_us: u64,
}

#[derive(Serialize)]
struct PressureReport {
    run_id: Uuid,
    agent_pid: u32,
    source: String,
    source_tree: String,
    engine_version: String,
    started_at_us: u64,
    ended_at_us: u64,
    overflow_returned_at_us: u64,
    elapsed_ms: u128,
    owner: PressureView,
    victim: PressureView,
    replacement: PressureView,
    heartbeat_before: PressureHeartbeat,
    heartbeat_after: PressureHeartbeat,
    committed: TerminalStatus,
    owner_grid: GeometryEvent,
    provider_after: TerminalStatus,
    fresh_status: TerminalStatus,
    healthy_output: PressureOutput,
    startup_redraw: bool,
    fresh_redraw: bool,
    fresh_input_submitted: bool,
    closure: TerminalClosure,
}

fn pressure_heartbeat(directory: &Path) -> Result<PressureHeartbeat> {
    decode_local_fixture(&fs::read(directory.join("pressure-heartbeat.json"))?)
}

fn wall_us() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_micros()
        .try_into()?)
}

async fn pressure_probe(args: Vec<String>) -> Result<()> {
    ensure!(
        args.len() == 10,
        "Expected --run-id, --state-dir, --runtime-id, --tmux-bin and --report"
    );
    let run_id = Uuid::parse_str(&option(&args, "--run-id")?)?;
    let directory = PathBuf::from(option(&args, "--state-dir")?);
    let tmux = PathBuf::from(option(&args, "--tmux-bin")?);
    let runtime = Uuid::parse_str(&option(&args, "--runtime-id")?)?;
    let report = PathBuf::from(option(&args, "--report")?);
    validate_directory(&directory)?;
    ensure!(
        cfg!(target_os = "macos")
            && directory == directory.canonicalize()?
            && directory.parent() == Some(Path::new("/private/tmp"))
            && directory
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| { name.starts_with("rs-p0-") || name.starts_with("rs-w1-") })
            && report == directory.join("pressure-probe.json"),
        "Pressure mode requires a private disposable lab report"
    );
    let heartbeat = pressure_heartbeat(&directory)?;
    let runtime_id = runtime.to_string();
    let bridge = DesktopBridge::new(Some(directory.clone()), tmux.clone());
    let initial = bridge.inventory().await?;
    ensure!(
        initial
            .runtimes
            .iter()
            .any(|entry| entry.runtime_id == runtime
                && entry.pane_pid == Some(heartbeat.pid)
                && !entry.exited),
        "Raw fixture is not the requested managed process"
    );

    let started = Instant::now();
    let started_at_us = wall_us()?;
    let owner = bridge.attach(&runtime_id).await?;
    let owner_process = PressureView::observe(&bridge, &owner).await?;
    let epoch = await_owner(&bridge, &owner).await?;
    let victim = bridge.attach_observer(&runtime_id).await?;
    let victim_process = PressureView::observe(&bridge, &victim).await?;
    let committed = async {
        pressure_bytes(&bridge, &victim, false, b"RSSTARTREADY").await?;
        let size = if owner.status.committed.size == NORMAL {
            NARROW
        } else {
            NORMAL
        };
        let ResizeResult::Committed { status, .. } =
            bridge.resize(&owner.view_id, size, epoch).await?
        else {
            bail!("Pressure geometry commit was unconfirmed");
        };
        Ok::<_, anyhow::Error>(status)
    }
    .await?;
    let owner_grid = await_grid(&bridge, &owner, &committed, true).await?;
    let heartbeat_before = pressure_heartbeat(&directory)?;
    ensure!(
        heartbeat_before.pid == heartbeat.pid && heartbeat_before.instance == heartbeat.instance,
        "Raw fixture changed before pressure"
    );
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(directory.join("pressure-bridge.trigger"))?;
    let peer = pressure_bytes(&bridge, &owner, true, b"RSBRIDGEKEPT");
    let overflow = async {
        let retired = tokio::time::timeout(Duration::from_secs(12), async {
            loop {
                if bridge.attachment_pid(&victim.view_id).is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        ensure!(retired.is_ok(), "overflow not reached");
        let chunk = bridge
            .read(&victim.view_id)
            .await
            .context("overflow not reached")?;
        let closure = TerminalClosure::Overflow { high_water: 16 };
        ensure!(chunk.closure == Some(closure), "overflow not reached");
        ensure!(
            chunk.closed
                && !chunk.writable
                && chunk.data.is_empty()
                && chunk.status.is_none()
                && chunk.geometry.is_none()
                && bridge.read(&victim.view_id).await.is_err(),
            "Overflow did not retire only the original scoped stream"
        );
        let at = wall_us()?;
        victim_process.wait_reaped().await?;
        Ok::<_, anyhow::Error>((closure, at))
    };
    let ((closure, overflow_returned_at_us), healthy_output) = tokio::try_join!(overflow, peer)?;
    let heartbeat_after = pressure_heartbeat(&directory)?;
    ensure!(
        heartbeat_after.pid == heartbeat_before.pid
            && heartbeat_after.instance == heartbeat_before.instance
            && heartbeat_after.sequence > heartbeat_before.sequence,
        "Independent heartbeat did not advance through Full"
    );
    let provider_after = wait_provider(&directory, &committed).await?;
    ensure!(
        provider_after.terminal.target == owner.status.terminal.target
            && provider_after.epoch == Some(epoch),
        "Owner, runtime, or provider changed through pressure"
    );

    let replacement = bridge.attach_observer(&runtime_id).await?;
    let replacement_process = PressureView::observe(&bridge, &replacement).await?;
    ensure!(
        replacement.view_id != victim.view_id
            && replacement.status.terminal.target == committed.terminal.target
            && replacement.status.committed == committed.committed,
        "Replacement did not attach to the current target and grid"
    );
    pressure_bytes(&bridge, &replacement, false, b"RSBRIDGEKEPT").await?;
    let incarnation = bridge
        .read(&owner.view_id)
        .await?
        .paste_incarnation
        .context("Pressure owner writer grant unavailable")?;
    bridge
        .input(
            &owner.view_id,
            b"RS-BRIDGE-FRESH\r".to_vec(),
            &incarnation.to_string(),
            false,
        )
        .await?;
    bridge.detach(&replacement.view_id);
    bridge.detach(&owner.view_id);
    tokio::try_join!(
        replacement_process.wait_reaped(),
        owner_process.wait_reaped()
    )?;

    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let repo = repo.to_str().context("Invalid repository path")?;
    let git = Path::new("/usr/bin/git");
    let result = PressureReport {
        run_id,
        agent_pid: initial.agent_pid,
        source: command(git, &["-C", repo, "rev-parse", "HEAD"]).await?,
        source_tree: command(git, &["-C", repo, "rev-parse", "HEAD^{tree}"]).await?,
        engine_version: command(&tmux, &["-V"]).await?,
        started_at_us,
        ended_at_us: wall_us()?,
        elapsed_ms: started.elapsed().as_millis(),
        overflow_returned_at_us,
        owner: owner_process,
        victim: victim_process,
        replacement: replacement_process,
        heartbeat_before,
        heartbeat_after,
        committed,
        owner_grid,
        provider_after,
        fresh_status: replacement.status,
        healthy_output,
        startup_redraw: true,
        fresh_redraw: true,
        fresh_input_submitted: true,
        closure,
    };
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(report)?;
    writeln!(out, "{}", encode_local_observation(&result)?)?;
    Ok(())
}

async fn pressure_bytes(
    bridge: &DesktopBridge,
    view: &Attachment,
    writable: bool,
    marker: &[u8],
) -> Result<PressureOutput> {
    tokio::time::timeout(Duration::from_secs(14), async {
        let mut tail = Vec::new();
        let mut total = 0;
        let mut first = None;
        loop {
            let chunk = bridge.read(&view.view_id).await?;
            ensure!(
                !chunk.closed && chunk.writable == writable && chunk.geometry.is_none(),
                "Healthy view lost stream or authority"
            );
            if chunk.data.is_empty() {
                continue;
            }
            let at = wall_us()?;
            let first_at_us = *first.get_or_insert(at);
            total += chunk.data.len();
            tail.extend(chunk.data);
            if tail.windows(marker.len()).any(|window| window == marker) {
                return Ok(PressureOutput {
                    bytes: total,
                    first_at_us,
                    last_at_us: at,
                });
            }
            let keep = tail.len().min(marker.len() - 1);
            tail.drain(..tail.len() - keep);
        }
    })
    .await
    .context("Actual terminal output witness did not arrive")?
}
