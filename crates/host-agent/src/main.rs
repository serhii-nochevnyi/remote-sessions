use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand};
use remote_sessions_host::{
    find_executable, initialize_catalog,
    model::{Request, Response},
    product_launch::managed_launch_argument,
    request, serve, serve_with_catalog,
};
use std::{os::unix::process::CommandExt, path::PathBuf};
use uuid::Uuid;

#[derive(Parser)]
#[command(
    version,
    about = "Local-only runtime spike. No remote execution or writable attachments."
)]
struct Cli {
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    #[arg(long, global = true, default_value = "tmux")]
    tmux: String,
    #[arg(long, global = true)]
    catalog_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Action,
}
#[derive(Subcommand)]
enum Action {
    CatalogInit,
    Doctor,
    Serve,
    Health,
    List,
    Create {
        #[arg(long)]
        session_id: Option<Uuid>,
        #[arg(long)]
        runtime_id: Option<Uuid>,
        #[arg(long)]
        cwd: PathBuf,
        #[arg(last = true, required = true)]
        argv: Vec<String>,
    },
    Snapshot {
        runtime_id: Uuid,
    },
    /// Attach read-only; Ctrl-b d detaches without stopping the runtime.
    Attach {
        runtime_id: Uuid,
    },
    /// Explicitly stop only this runtime, provided the pane ID still matches.
    Stop {
        runtime_id: Uuid,
        #[arg(long)]
        expected_pane_id: String,
    },
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if let Some(result) = managed_launch_argument(&args) {
        return result;
    }
    cli_main()
}

#[tokio::main]
async fn cli_main() -> Result<()> {
    let cli = Cli::parse();
    if matches!(cli.command, Action::CatalogInit) {
        return initialize_catalog(
            cli.catalog_dir
                .as_deref()
                .context("--catalog-dir is required")?,
        );
    }
    if matches!(cli.command, Action::Doctor) {
        let dependencies: Vec<_> = [&*cli.tmux, "claude", "codex"]
            .into_iter()
            .map(
                |name| serde_json::json!({"name":name,"available":find_executable(name).is_some()}),
            )
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(
                &serde_json::json!({"platform":std::env::consts::OS,"architecture":std::env::consts::ARCH,"stage":"local_runtime_spike","dependencies":dependencies,"remoteExecutionEnabled":false})
            )?
        );
        return Ok(());
    }
    let directory = cli
        .state_dir
        .context("--state-dir is required (private directory created by the runtime lab)")?;
    if matches!(cli.command, Action::Serve) {
        let tmux =
            find_executable(&cli.tmux).context("tmux executable not found; install tmux first")?;
        if let Some(catalog) = cli.catalog_dir {
            return serve_with_catalog(directory, tmux, catalog).await;
        }
        return serve(directory, tmux).await;
    }
    let attaching = matches!(cli.command, Action::Attach { .. });
    let message = match cli.command {
        Action::Health => Request::Health {},
        Action::List => Request::List {},
        Action::Create {
            session_id,
            runtime_id,
            cwd,
            argv,
        } => Request::Create {
            session_id: session_id.unwrap_or_else(Uuid::new_v4),
            runtime_id: runtime_id.unwrap_or_else(Uuid::new_v4),
            cwd,
            argv,
        },
        Action::Snapshot { runtime_id } => Request::Snapshot { runtime_id },
        Action::Attach { runtime_id } => Request::Attach { runtime_id },
        Action::Stop {
            runtime_id,
            expected_pane_id,
        } => Request::Stop {
            runtime_id,
            expected_pane_id,
        },
        Action::CatalogInit | Action::Serve | Action::Doctor => unreachable!(),
    };
    let response = request(&directory, &message).await?;
    if let Response::Error { message } = &response {
        bail!("{message}");
    }
    if attaching {
        let Response::Attachment { tmux_name } = response else {
            bail!("unexpected attachment response")
        };
        ensure!(
            remote_sessions_host::model::parse_name(&tmux_name).is_some(),
            "invalid attachment target"
        );
        let tmux = find_executable(&cli.tmux).context("tmux executable not found")?;
        // This local diagnostic viewer has no input capability and never resizes the runtime.
        let error = std::process::Command::new(tmux)
            .args(["-N", "-S"])
            .arg(directory.join("tmux.sock"))
            .args(["attach-session", "-r", "-E", "-t", &format!("={tmux_name}")])
            .env_remove("TMUX")
            .exec();
        return Err(error.into());
    }
    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}
