//! Uses the same bridge as Tauri against an already running local lab.
use remote_sessions_desktop_bridge::DesktopBridge;
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let directory = PathBuf::from(args.next().expect("state directory"));
    let runtime = args.next().expect("runtime id");
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
        while Instant::now() < deadline {
            let chunk = bridge.read(&view.view_id).await?;
            assert!(!chunk.closed);
            assert!(chunk.data.len() <= 8192);
            output.extend(chunk.data);
            if chunk.writable && String::from_utf8_lossy(&output).contains("P0 runtime alive") {
                break;
            }
        }
        assert!(String::from_utf8_lossy(&output).contains("P0 runtime alive"));
        bridge
            .input(
                &view.view_id,
                "Native bridge — Україна\r".as_bytes().to_vec(),
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
