#[tauri::command]
fn scaffold_status() -> remote_sessions_core::Status {
    remote_sessions_core::scaffold_status()
}

#[cfg(target_os = "macos")]
mod desktop {
    use remote_sessions_desktop_bridge::{
        Attachment, Chunk, CreateSubmission, DesktopBridge, GeometryEvent, GridAcknowledgement,
        Inventory, LocalCreateStatus, PasteAction, PasteResult, ProductIssue, ProviderProfile,
        ResizeEpoch, ResizeResult, TerminalSize, TerminalStatus, WorkspaceReadiness,
        WorkspaceSelection, encode_local_observation, encode_renderer_observation,
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
    pub async fn local_create_session(
        workspace_id: String,
        profile: ProviderProfile,
        separate_from: Option<String>,
        continue_reference: Option<String>,
        bridge: State<'_, DesktopBridge>,
    ) -> Result<CreateSubmission, String> {
        Ok(bridge
            .create_session(
                &workspace_id,
                profile,
                separate_from.as_deref(),
                continue_reference.as_deref(),
            )
            .await)
    }

    #[tauri::command]
    pub async fn local_create_status(
        reference: Option<String>,
        presented: Option<bool>,
        bridge: State<'_, DesktopBridge>,
    ) -> Result<LocalCreateStatus, String> {
        if presented == Some(true) {
            let reference = reference.as_deref().ok_or("Не вказано запис створення.")?;
            return Ok(bridge.acknowledge_create(reference).await);
        }
        Ok(bridge.create_status(reference.as_deref()).await)
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
    pub async fn local_window_fullscreen(
        window: WebviewWindow,
        action: String,
        intent: Option<String>,
    ) -> Result<bool, String> {
        let (reply, mut response) = async_runtime::channel(1);
        let invoking_window = window.clone();
        window
            .run_on_main_thread(move || {
                let window = invoking_window;
                let result = match (action.as_str(), intent) {
                    ("read", None) => fullscreen::read(&window),
                    ("enter", None) => window
                        .set_fullscreen(true)
                        .map_err(|e| e.to_string())
                        .and_then(|()| fullscreen::read(&window)),
                    ("exit", Some(intent)) => match fullscreen::exit(window, intent, reply.clone())
                    {
                        Ok(true) => return,
                        outcome => outcome,
                    },
                    ("cancelExit", Some(intent)) => fullscreen::cancel(&window, &intent),
                    ("focusAfterExit", Some(intent)) => fullscreen::focus(&window, &intent),
                    _ => Err("Некоректна дія повноекранного режиму.".into()),
                };
                let _ = reply.try_send(result);
            })
            .map_err(|e| e.to_string())?;
        response.recv().await.ok_or("Вікно недоступне.")?
    }

    mod fullscreen {
        use std::{
            cell::{OnceCell, RefCell},
            collections::HashMap,
            ffi::{CStr, c_char, c_void},
            mem, ptr,
        };
        use tauri::{Webview, WebviewWindow, Wry, async_runtime::Sender};

        type Object = *mut c_void;
        type Selector = *const c_void;
        type NotificationHandler = extern "C" fn(Object, Selector, Object);
        type ExitReply = Sender<Result<bool, String>>;
        #[cfg(target_arch = "aarch64")]
        type ObjcBool = bool;
        #[cfg(target_arch = "x86_64")]
        type ObjcBool = i8;

        #[link(name = "objc")]
        unsafe extern "C" {
            fn objc_getClass(name: *const c_char) -> Object;
            fn objc_allocateClassPair(
                superclass: Object,
                name: *const c_char,
                extra: usize,
            ) -> Object;
            fn objc_disposeClassPair(class: Object);
            fn objc_registerClassPair(class: Object);
            fn class_addMethod(
                class: Object,
                selector: Selector,
                imp: NotificationHandler,
                types: *const c_char,
            ) -> ObjcBool;
            fn sel_registerName(name: *const c_char) -> Selector;
            fn objc_msgSend();
            fn objc_release(object: Object);
        }

        #[link(name = "Foundation", kind = "framework")]
        #[link(name = "AppKit", kind = "framework")]
        unsafe extern "C" {
            static NSWindowDidExitFullScreenNotification: Object;
            static NSWindowWillCloseNotification: Object;
            static NSWindowDidBecomeKeyNotification: Object;
            static NSApplicationDidResignActiveNotification: Object;
        }

        unsafe extern "C" {
            static _dispatch_main_q: u8;
            fn dispatch_time(when: u64, delta: i64) -> u64;
            fn dispatch_async_f(queue: Object, context: Object, work: extern "C" fn(Object));
            fn dispatch_after_f(
                when: u64,
                queue: Object,
                context: Object,
                work: extern "C" fn(Object),
            );
        }

        struct Observer {
            center: Object,
            object: Object,
        }

        impl Observer {
            fn new(window: Object) -> Result<Self, String> {
                let class =
                    OBSERVER_CLASS.with(|class| *class.get_or_init(register_observer_class));
                // @invariant: AppKit-thread registration owns the +new observer until Drop.
                unsafe {
                    let object = send_object(class, c"new");
                    if object.is_null() {
                        return Err("Не вдалося спостерігати за режимом вікна.".into());
                    }
                    let center = send_object(
                        objc_getClass(c"NSNotificationCenter".as_ptr()),
                        c"defaultCenter",
                    );
                    let observer = Self { center, object };
                    let add: unsafe extern "C" fn(
                        Object,
                        Selector,
                        Object,
                        Selector,
                        Object,
                        Object,
                    ) = mem::transmute(objc_msgSend as unsafe extern "C" fn());
                    for (handler, name, source) in [
                        (
                            c"remoteSessionsDidExit:",
                            NSWindowDidExitFullScreenNotification,
                            window,
                        ),
                        (
                            c"remoteSessionsWillClose:",
                            NSWindowWillCloseNotification,
                            window,
                        ),
                        (
                            c"remoteSessionsFocusChanged:",
                            NSWindowDidBecomeKeyNotification,
                            ptr::null_mut(),
                        ),
                        (
                            c"remoteSessionsFocusChanged:",
                            NSApplicationDidResignActiveNotification,
                            ptr::null_mut(),
                        ),
                    ] {
                        add(
                            center,
                            sel_registerName(c"addObserver:selector:name:object:".as_ptr()),
                            object,
                            sel_registerName(handler.as_ptr()),
                            name,
                            source,
                        );
                    }
                    Ok(observer)
                }
            }

            fn remove_exit_notification(&self, window: Object) {
                unsafe {
                    let remove: unsafe extern "C" fn(Object, Selector, Object, Object, Object) =
                        mem::transmute(objc_msgSend as unsafe extern "C" fn());
                    remove(
                        self.center,
                        sel_registerName(c"removeObserver:name:object:".as_ptr()),
                        self.object,
                        NSWindowDidExitFullScreenNotification,
                        window,
                    );
                }
            }
        }

        impl Drop for Observer {
            fn drop(&mut self) {
                // @invariant: Unregister the observer on the AppKit thread before releasing it.
                unsafe {
                    let remove: unsafe extern "C" fn(Object, Selector, Object) =
                        mem::transmute(objc_msgSend as unsafe extern "C" fn());
                    remove(
                        self.center,
                        sel_registerName(c"removeObserver:".as_ptr()),
                        self.object,
                    );
                    objc_release(self.object);
                }
            }
        }

        struct PendingExit {
            window: WebviewWindow,
            intent: String,
            observer: Observer,
            notified: bool,
            // @invariant: A consumed reply marks settled readback and enables one focus request.
            reply: Option<ExitReply>,
        }

        thread_local! {
            static OBSERVER_CLASS: OnceCell<Object> = const { OnceCell::new() };
            static EXITS: RefCell<HashMap<Object, PendingExit>> = RefCell::new(HashMap::new());
        }

        unsafe fn send_object(receiver: Object, selector: &CStr) -> Object {
            // @contract: The selector takes no arguments and returns an Objective-C object.
            unsafe {
                let send: unsafe extern "C" fn(Object, Selector) -> Object =
                    mem::transmute(objc_msgSend as unsafe extern "C" fn());
                send(receiver, sel_registerName(selector.as_ptr()))
            }
        }

        fn register_observer_class() -> Object {
            // @contract: Notification handlers have the Objective-C signature void(id, SEL, id).
            unsafe {
                let class = objc_allocateClassPair(
                    objc_getClass(c"NSObject".as_ptr()),
                    c"RSFullscreenExitObserver".as_ptr(),
                    0,
                );
                if class.is_null() {
                    return class;
                }
                for (selector, handler) in [
                    (c"remoteSessionsDidExit:", did_exit as NotificationHandler),
                    (c"remoteSessionsWillClose:", will_close),
                    (c"remoteSessionsFocusChanged:", focus_changed),
                ] {
                    if class_addMethod(
                        class,
                        sel_registerName(selector.as_ptr()),
                        handler,
                        c"v@:@".as_ptr(),
                    ) as u8
                        == 0
                    {
                        objc_disposeClassPair(class);
                        return ptr::null_mut();
                    }
                }
                objc_registerClassPair(class);
                class
            }
        }

        fn on_main_queue<F: FnOnce() + 'static>(delay_ns: i64, work: F) {
            extern "C" fn run<F: FnOnce()>(context: Object) {
                // @invariant: The dispatch callback consumes its Box<F> exactly once.
                let work = unsafe { Box::from_raw(context.cast::<F>()) };
                work();
            }
            let context = Box::into_raw(Box::new(work)).cast();
            // @invariant: The dispatch callback and its capture stay on the AppKit thread.
            unsafe {
                let queue = (&raw const _dispatch_main_q).cast_mut().cast();
                if delay_ns == 0 {
                    dispatch_async_f(queue, context, run::<F>);
                } else {
                    dispatch_after_f(dispatch_time(0, delay_ns), queue, context, run::<F>);
                }
            }
        }

        fn observed_exit(observer: Object) -> Option<(Object, String)> {
            EXITS.with_borrow(|exits| {
                exits.iter().find_map(|(&window, exit)| {
                    (exit.observer.object == observer).then(|| (window, exit.intent.clone()))
                })
            })
        }

        extern "C" fn did_exit(observer: Object, _: Selector, _: Object) {
            let observed = EXITS.with_borrow_mut(|exits| {
                exits.iter_mut().find_map(|(&window, exit)| {
                    if exit.observer.object != observer || exit.notified {
                        return None;
                    }
                    exit.notified = true;
                    Some((window, exit.intent.clone()))
                })
            });
            if let Some((window, intent)) = observed {
                // @invariant: Settled readback runs after the AppKit notification callback unwinds.
                on_main_queue(0, move || settle(window, &intent));
            }
        }

        extern "C" fn will_close(observer: Object, _: Selector, _: Object) {
            if let Some((window, intent)) = observed_exit(observer) {
                cancel_from_notification(window, &intent);
            }
        }

        extern "C" fn focus_changed(observer: Object, _: Selector, notification: Object) {
            if let Some((window, intent)) = observed_exit(observer) {
                let source = unsafe { send_object(notification, c"object") };
                if source != window {
                    cancel_from_notification(window, &intent);
                }
            }
        }

        fn take_exit(window: Object, intent: &str) -> Option<PendingExit> {
            EXITS.with_borrow_mut(|exits| {
                let exit = exits.get(&window)?;
                if exit.intent == intent {
                    exits.remove(&window)
                } else {
                    None
                }
            })
        }

        fn complete(exit: PendingExit, result: Result<bool, String>) {
            drop(exit.observer);
            if let Some(reply) = exit.reply {
                let _ = reply.try_send(result);
            }
        }

        fn finish(window: Object, intent: &str, result: Result<bool, String>) {
            if let Some(exit) = take_exit(window, intent) {
                complete(exit, result);
            }
        }

        fn cancel_from_notification(window: Object, intent: &str) {
            if let Some(exit) = take_exit(window, intent) {
                // @invariant: Revoke eligibility now and release the observer after its callback.
                on_main_queue(0, move || complete(exit, Ok(false)));
            }
        }

        fn native_window(window: &WebviewWindow) -> Result<Object, String> {
            let native = window.ns_window().map_err(|e| e.to_string())?;
            if native.is_null() {
                Err("Вікно недоступне.".into())
            } else {
                Ok(native)
            }
        }

        pub fn read(window: &WebviewWindow) -> Result<bool, String> {
            let native = native_window(window)?;
            // @contract: AppKit's NSWindowStyleMask is an NSUInteger with FullScreen at bit 14.
            let style = unsafe {
                let send: unsafe extern "C" fn(Object, Selector) -> usize =
                    mem::transmute(objc_msgSend as unsafe extern "C" fn());
                send(native, sel_registerName(c"styleMask".as_ptr()))
            };
            Ok(style & (1 << 14) != 0)
        }

        pub fn exit(
            window: WebviewWindow,
            intent: String,
            reply: ExitReply,
        ) -> Result<bool, String> {
            let native = native_window(&window)?;
            if EXITS.with_borrow(|exits| exits.contains_key(&native)) {
                return Err("Режим вікна вже змінюється.".into());
            }
            if !window.is_fullscreen().map_err(|e| e.to_string())? || !read(&window)? {
                return Ok(false);
            }

            let observer = Observer::new(native)?;
            EXITS.with_borrow_mut(|exits| {
                exits.insert(
                    native,
                    PendingExit {
                        window: window.clone(),
                        intent: intent.clone(),
                        observer,
                        notified: false,
                        reply: Some(reply),
                    },
                )
            });
            if let Err(error) = window.set_fullscreen(false) {
                drop(take_exit(native, &intent));
                return Err(error.to_string());
            }
            on_main_queue(5_000_000_000, move || {
                finish(
                    native,
                    &intent,
                    Err("Не вдалося підтвердити вихід із повноекранного режиму.".into()),
                );
            });
            Ok(true)
        }

        fn settle(native: Object, intent: &str) {
            let window = EXITS.with_borrow(|exits| {
                exits
                    .get(&native)
                    .filter(|exit| exit.intent == intent)
                    .map(|exit| exit.window.clone())
            });
            let Some(window) = window else {
                return;
            };
            let result = (|| {
                if read(&window)? || window.is_fullscreen().map_err(|e| e.to_string())? {
                    return Err("Вікно залишилося в повноекранному режимі.".into());
                }
                window.is_focused().map_err(|e| e.to_string())
            })();
            if result != Ok(true) {
                finish(native, intent, result);
                return;
            }

            let reply = EXITS.with_borrow_mut(|exits| {
                let exit = exits
                    .get_mut(&native)
                    .filter(|exit| exit.intent == intent)?;
                exit.observer.remove_exit_notification(native);
                exit.reply.take()
            });
            if let Some(reply) = reply
                && reply.try_send(Ok(true)).is_err()
            {
                drop(take_exit(native, intent));
            }
        }

        pub fn cancel(window: &WebviewWindow, intent: &str) -> Result<bool, String> {
            finish(native_window(window)?, intent, Ok(false));
            Ok(false)
        }

        pub fn focus(window: &WebviewWindow, intent: &str) -> Result<bool, String> {
            let native = native_window(window)?;
            let settled = EXITS.with_borrow(|exits| {
                exits
                    .get(&native)
                    .is_some_and(|exit| exit.intent == intent && exit.reply.is_none())
            });
            if !settled {
                return Ok(false);
            }
            drop(take_exit(native, intent));
            if !window.is_focused().map_err(|e| e.to_string())? || read(window)? {
                return Ok(false);
            }
            let webview: &Webview<Wry> = window.as_ref();
            webview.set_focus().map_err(|e| e.to_string())?;
            Ok(true)
        }
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
        let trace = resize_trace();
        let chunk = bridge.read(&view_id).await.map_err(|error| {
            let message = error.to_string();
            if trace {
                trace_stream_failure(&view_id, "read", bridge_failure_reason(&message));
            }
            message
        })?;
        if trace {
            if chunk.closed {
                trace_stream_failure(&view_id, "read", "stream_closed");
            }
            if let Some(status) = &chunk.status
                && status.committed.revision > 0
                && let Ok(line) = encode_local_observation(status)
            {
                eprintln!("RS_W1_RESIZE_PROVIDER {line}");
            }
        }
        Ok(chunk)
    }
    #[tauri::command]
    pub async fn terminal_status(
        view_id: String,
        bridge: State<'_, DesktopBridge>,
    ) -> Result<TerminalStatus, String> {
        bridge.status(&view_id).await.map_err(|e| e.to_string())
    }
    #[tauri::command]
    pub async fn terminal_resize(
        view_id: String,
        epoch: ResizeEpoch,
        size: TerminalSize,
        bridge: State<'_, DesktopBridge>,
    ) -> Result<ResizeResult, String> {
        bridge
            .resize(&view_id, size, epoch)
            .await
            .map_err(|e| e.to_string())
    }
    #[tauri::command]
    pub async fn terminal_acknowledge_grid(
        view_id: String,
        acknowledgement: GridAcknowledgement,
        bridge: State<'_, DesktopBridge>,
    ) -> Result<GeometryEvent, String> {
        let trace = resize_trace();
        let event = bridge
            .acknowledge_grid(&view_id, acknowledgement)
            .await
            .map_err(|error| {
                let message = error.to_string();
                if trace {
                    trace_stream_failure(&view_id, "acknowledge", bridge_failure_reason(&message));
                }
                message
            })?;
        if trace && let Ok(line) = encode_renderer_observation(&event, acknowledgement) {
            eprintln!("RS_W1_RESIZE_GRID {line}");
        }
        Ok(event)
    }
    fn resize_trace() -> bool {
        cfg!(debug_assertions) && std::env::var("RS_W1_RESIZE_TRACE").as_deref() == Ok("1")
    }

    fn bridge_failure_reason(message: &str) -> &'static str {
        match message {
            "View detached" => "view_detached",
            "View lock unavailable" => "view_lock_unavailable",
            "Terminal read already in progress" => "read_in_progress",
            "Terminal grid acknowledgement required" => "grid_ack_required",
            "Terminal output did not reach a geometry boundary" => "geometry_drain_timeout",
            "Terminal attachment identity changed" => "attachment_identity_changed",
            "Розмір термінала недоступний" => {
                "terminal_status_unavailable"
            }
            "Terminal geometry revision went backwards" => "geometry_revision_regressed",
            "Native PTY unavailable" => "native_pty_unavailable",
            "Native PTY resize was not observed" => "native_pty_resize_unobserved",
            "Tmux client resize was not observed" => "tmux_client_resize_unobserved",
            "Terminal boundary already in progress" => "boundary_in_progress",
            "No pending geometry boundary" => "boundary_missing",
            "Terminal renderer acknowledgement is stale or mismatched" => "grid_ack_mismatch",
            "Terminal geometry boundary was superseded" => "boundary_superseded",
            "Attachment PTY descriptor unavailable" => "attachment_pty_unavailable",
            "Attachment client unavailable" | "Exact tmux attachment client unavailable" => {
                "tmux_client_unavailable"
            }
            "Ambiguous attachment client" => "tmux_client_ambiguous",
            "Invalid tmux client grid" => "tmux_client_grid_invalid",
            "Attachment grid changed" => "attachment_grid_changed",
            "IPC read timeout" => "agent_read_timeout",
            "IPC frame is incomplete or exceeds limit" | "invalid agent response" => {
                "agent_response_invalid"
            }
            "local agent is unavailable" => "agent_unavailable",
            "deadline has elapsed" => "bridge_operation_timeout",
            _ => "bridge_error",
        }
    }

    fn trace_stream_failure(view_id: &str, stage: &str, reason: &str) {
        use std::io::Write;

        let uuid = view_id.len() == 36
            && view_id.bytes().enumerate().all(|(index, byte)| {
                if matches!(index, 8 | 13 | 18 | 23) {
                    byte == b'-'
                } else {
                    byte.is_ascii_hexdigit()
                }
            });
        if uuid {
            let _ = writeln!(
                std::io::stderr().lock(),
                "RS_W1_RESIZE_FAILURE {{\"view_id\":\"{view_id}\",\"stage\":\"{stage}\",\"reason\":\"{reason}\"}}"
            );
        }
    }
    #[tauri::command]
    pub async fn terminal_input(
        view_id: String,
        data: Vec<u8>,
        incarnation: Option<String>,
        paste: Option<bool>,
        bridge: State<'_, DesktopBridge>,
    ) -> Result<(), String> {
        ordinary_input(paste, async move || {
            let incarnation = incarnation.as_deref().ok_or("Керування ще не отримано")?;
            bridge
                .input(&view_id, data, incarnation, false)
                .await
                .map_err(|e| e.to_string())
        })
        .await
    }

    async fn ordinary_input(
        paste: Option<bool>,
        dispatch: impl AsyncFnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        if paste == Some(true) {
            return Err("PasteUnavailable".into());
        }
        dispatch().await
    }

    #[tauri::command]
    pub async fn terminal_paste_stage(
        view_id: String,
        incarnation: String,
        operation: u64,
        offset: usize,
        data: Vec<u8>,
        bridge: State<'_, DesktopBridge>,
    ) -> Result<PasteResult, String> {
        Ok(bridge
            .paste(
                &view_id,
                &incarnation,
                operation,
                PasteAction::Stage { offset, data },
            )
            .await)
    }

    #[tauri::command]
    pub async fn terminal_paste_commit(
        view_id: String,
        incarnation: String,
        operation: u64,
        bytes: usize,
        bridge: State<'_, DesktopBridge>,
    ) -> Result<PasteResult, String> {
        Ok(bridge
            .paste(
                &view_id,
                &incarnation,
                operation,
                PasteAction::Commit { bytes },
            )
            .await)
    }

    #[tauri::command]
    pub async fn terminal_paste_ack(
        view_id: String,
        incarnation: String,
        operation: u64,
        receipt: String,
        bridge: State<'_, DesktopBridge>,
    ) -> Result<PasteResult, String> {
        Ok(bridge
            .paste(
                &view_id,
                &incarnation,
                operation,
                PasteAction::Ack { receipt },
            )
            .await)
    }

    #[cfg(test)]
    #[test]
    fn terminal_input_rejects_legacy_paste() {
        tauri::async_runtime::block_on(async {
            let mut calls = 0;
            for paste in [Some(true), Some(false), None] {
                let result = ordinary_input(paste, async || {
                    calls += 1;
                    Ok(())
                })
                .await;
                match paste {
                    Some(true) => {
                        assert_eq!(result, Err("PasteUnavailable".into()));
                        assert_eq!(calls, 0);
                    }
                    _ => assert_eq!(result, Ok(())),
                }
            }
            assert_eq!(calls, 2);
        });
    }
    #[tauri::command]
    pub fn terminal_detach(
        view_id: String,
        failure: Option<String>,
        bridge: State<'_, DesktopBridge>,
    ) {
        bridge.detach(&view_id);
        if resize_trace()
            && let Some(
                reason @ ("attachment_setup"
                | "read_response"
                | "grid_ack_response"
                | "geometry_routing"
                | "stream_closed"
                | "terminal_status_changed"
                | "geometry_revision_not_new"
                | "geometry_status_rejected"
                | "geometry_output_incomplete"
                | "renderer_grid_mismatch"
                | "stale_grid_ack"
                | "renderer_write_timeout"),
            ) = failure.as_deref()
        {
            trace_stream_failure(&view_id, "renderer_detach", reason);
        }
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
                desktop::local_window_fullscreen,
                desktop::local_pick_workspace,
                desktop::local_workspace_readiness,
                desktop::local_create_session,
                desktop::local_create_status,
                desktop::terminal_attach,
                desktop::terminal_read,
                desktop::terminal_status,
                desktop::terminal_resize,
                desktop::terminal_acknowledge_grid,
                desktop::terminal_detach,
                desktop::terminal_input,
                desktop::terminal_paste_stage,
                desktop::terminal_paste_commit,
                desktop::terminal_paste_ack
            ])
    };
    #[cfg(not(target_os = "macos"))]
    let builder = builder.invoke_handler(tauri::generate_handler![scaffold_status]);
    builder
        .run(tauri::generate_context!())
        .expect("failed to run Remote Sessions client");
}
