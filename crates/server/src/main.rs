#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:7331").await?;
    eprintln!("scaffold server: http://127.0.0.1:7331/healthz (no session or relay endpoints)");
    axum::serve(listener, remote_sessions_server::app())
        .with_graceful_shutdown(async {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("SIGTERM handler");
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
        })
        .await?;
    Ok(())
}
