#[tauri::command]
fn scaffold_status() -> remote_sessions_core::Status {
    remote_sessions_core::scaffold_status()
}

#[cfg(target_os = "macos")]
mod desktop {
    use remote_sessions_desktop_bridge::{Attachment, Chunk, DesktopBridge, Inventory};
    use tauri::State;
    #[tauri::command]
    pub async fn local_inventory(bridge: State<'_, DesktopBridge>) -> Result<Inventory, String> {
        bridge.inventory().await.map_err(|e| e.to_string())
    }
    #[tauri::command]
    pub async fn terminal_attach(
        runtime_id: String,
        bridge: State<'_, DesktopBridge>,
    ) -> Result<Attachment, String> {
        bridge.attach(&runtime_id).await.map_err(|e| e.to_string())
    }
    #[tauri::command]
    pub async fn terminal_read(
        view_id: String,
        bridge: State<'_, DesktopBridge>,
    ) -> Result<Chunk, String> {
        bridge.read(&view_id).await.map_err(|e| e.to_string())
    }
    #[tauri::command]
    pub async fn terminal_input(
        view_id: String,
        data: Vec<u8>,
        paste: bool,
        bridge: State<'_, DesktopBridge>,
    ) -> Result<(), String> {
        bridge
            .input(&view_id, data, paste)
            .await
            .map_err(|e| e.to_string())
    }
    #[tauri::command]
    pub fn terminal_detach(view_id: String, bridge: State<'_, DesktopBridge>) {
        bridge.detach(&view_id);
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default();
    #[cfg(target_os = "macos")]
    let builder = {
        use remote_sessions_desktop_bridge::DesktopBridge;
        use tauri::Manager;
        let directory = std::env::var_os("RS_STATE_DIR").map(std::path::PathBuf::from);
        let tmux = std::env::var_os("RS_TMUX_BIN")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| "/opt/homebrew/bin/tmux".into());
        builder
            .manage(DesktopBridge::new(directory, tmux))
            .on_page_load(|webview, payload| {
                if payload.event() == tauri::webview::PageLoadEvent::Started {
                    webview.state::<DesktopBridge>().detach_all();
                }
            })
            .on_window_event(|window, event| {
                if matches!(event, tauri::WindowEvent::Destroyed) {
                    window.state::<DesktopBridge>().detach_all();
                }
            })
            .invoke_handler(tauri::generate_handler![
                scaffold_status,
                desktop::local_inventory,
                desktop::terminal_attach,
                desktop::terminal_read,
                desktop::terminal_detach,
                desktop::terminal_input
            ])
    };
    #[cfg(not(target_os = "macos"))]
    let builder = builder.invoke_handler(tauri::generate_handler![scaffold_status]);
    builder
        .run(tauri::generate_context!())
        .expect("failed to run Remote Sessions client");
}
