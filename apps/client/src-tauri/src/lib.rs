#[tauri::command]
fn scaffold_status() -> remote_sessions_core::Status {
    remote_sessions_core::scaffold_status()
}

#[cfg(target_os = "macos")]
mod desktop {
    use remote_sessions_desktop_bridge::{
        Attachment, Chunk, DesktopBridge, Inventory, ProductIssue, ProviderProfile,
        WorkspaceReadiness, WorkspaceSelection,
    };
    use tauri::{State, WebviewWindow, async_runtime};
    use tauri_plugin_dialog::{DialogExt, FilePath};

    #[derive(Default)]
    pub struct WorkspacePicker(async_runtime::Mutex<()>);

    #[tauri::command]
    pub async fn local_pick_workspace(
        window: WebviewWindow,
        picker: State<'_, WorkspacePicker>,
        bridge: State<'_, DesktopBridge>,
    ) -> Result<WorkspaceSelection, String> {
        let _selection = picker
            .0
            .try_lock()
            .map_err(|_| "Вибір папки вже відкрито.")?;
        let (sender, mut receiver) = async_runtime::channel(1);
        window
            .dialog()
            .file()
            .set_title("Виберіть папку проєкту")
            .set_parent(&window)
            .set_can_create_directories(false)
            .pick_folder(move |choice| {
                let _ = sender.try_send(choice);
            });
        let picked = match receiver
            .recv()
            .await
            .ok_or("Не вдалося відкрити вибір папки.")?
        {
            None => None,
            Some(FilePath::Path(path)) => Some(path),
            Some(_) => {
                return Ok(WorkspaceSelection::Blocked {
                    issue: ProductIssue::UnsupportedPath,
                });
            }
        };
        bridge
            .pick_workspace(picked)
            .await
            .map_err(|_| "Не вдалося підтвердити папку в локальному агенті.".into())
    }

    #[tauri::command]
    pub async fn local_workspace_readiness(
        workspace_id: String,
        profile: ProviderProfile,
        bridge: State<'_, DesktopBridge>,
    ) -> Result<WorkspaceReadiness, String> {
        bridge
            .workspace_readiness(&workspace_id, profile)
            .await
            .map_err(|_| "Не вдалося отримати результат від локального агента.".into())
    }

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
            .plugin(tauri_plugin_dialog::init())
            .manage(desktop::WorkspacePicker::default())
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
                desktop::local_pick_workspace,
                desktop::local_workspace_readiness,
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
