use crate::model::{Request, Response, Runtime, parse_name, tmux_name};
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
            });
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
                ensure!(runtimes.len() < 16, "local spike limit is 16 runtimes");
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
                self.command(&["set-option", "-t", &name, "@rs_command", label])
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
