use aissh_config::{Config, Paths};
use aissh_protocol::{
    PROTOCOL_VERSION, Request, RequestFrame, Response, ResponseData, read_frame, write_frame,
};
use std::sync::atomic::{AtomicU64, Ordering};
use tauri::{
    Manager, WebviewUrl, WebviewWindowBuilder,
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
};
use tokio::net::UnixStream;

struct AppState {
    paths: Paths,
    request_id: AtomicU64,
}

impl AppState {
    async fn call(&self, request: Request) -> Result<ResponseData, String> {
        let mut stream = UnixStream::connect(&self.paths.socket)
            .await
            .map_err(|e| format!("aisshd unavailable: {e}"))?;
        let handshake = self.request_id.fetch_add(1, Ordering::Relaxed);
        write_frame(
            &mut stream,
            &RequestFrame {
                request_id: handshake,
                request: Request::Handshake {
                    protocol_version: PROTOCOL_VERSION,
                    client_name: "AI SSH Menubar".into(),
                },
            },
        )
        .await
        .map_err(|e| e.to_string())?;
        let response: Response = read_frame(&mut stream).await.map_err(|e| e.to_string())?;
        response.result.map_err(|e| e.to_string())?;
        let id = self.request_id.fetch_add(1, Ordering::Relaxed);
        write_frame(
            &mut stream,
            &RequestFrame {
                request_id: id,
                request,
            },
        )
        .await
        .map_err(|e| e.to_string())?;
        let response: Response = read_frame(&mut stream).await.map_err(|e| e.to_string())?;
        response.result.map_err(|e| e.to_string())
    }
}

#[tauri::command]
async fn sessions(
    state: tauri::State<'_, AppState>,
    include_history: bool,
) -> Result<ResponseData, String> {
    state.call(Request::SessionsList { include_history }).await
}
#[tauri::command]
async fn targets(state: tauri::State<'_, AppState>) -> Result<ResponseData, String> {
    state.call(Request::TargetsList).await
}
#[tauri::command]
async fn test_target(
    state: tauri::State<'_, AppState>,
    target_id: String,
) -> Result<ResponseData, String> {
    let created = state
        .call(Request::SessionCreate {
            target_id,
            purpose: "Menubar connection test".into(),
        })
        .await?;
    if let ResponseData::Session(info) = &created {
        state
            .call(Request::SessionClose {
                session_id: info.id.clone(),
            })
            .await?;
    }
    Ok(created)
}
#[tauri::command]
async fn session_status(
    state: tauri::State<'_, AppState>,
    session_id: String,
) -> Result<ResponseData, String> {
    state.call(Request::SessionStatus { session_id }).await
}
#[tauri::command]
async fn session_events(
    state: tauri::State<'_, AppState>,
    session_id: String,
    after_sequence: u64,
    max_bytes: usize,
) -> Result<ResponseData, String> {
    state
        .call(Request::ShellRead {
            session_id,
            after_sequence,
            max_bytes,
        })
        .await
}
#[tauri::command]
async fn reload_config(state: tauri::State<'_, AppState>) -> Result<ResponseData, String> {
    state.call(Request::ReloadConfig).await
}
#[tauri::command]
fn retention_days(state: tauri::State<'_, AppState>) -> Result<u32, String> {
    Config::load(&state.paths)
        .map(|v| v.retention_days)
        .map_err(|e| e.to_string())
}
#[tauri::command]
async fn set_retention_days(
    state: tauri::State<'_, AppState>,
    days: u32,
) -> Result<ResponseData, String> {
    if !(1..=3650).contains(&days) {
        return Err("retention must be between 1 and 3650 days".into());
    }
    let mut config = Config::load(&state.paths).map_err(|e| e.to_string())?;
    config.retention_days = days;
    config
        .save_atomic(&state.paths)
        .map_err(|e| e.to_string())?;
    state.call(Request::ReloadConfig).await
}
#[tauri::command]
fn open_config(state: tauri::State<'_, AppState>) -> Result<(), String> {
    std::process::Command::new("open")
        .arg(&state.paths.root)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn open_session(app: &tauri::AppHandle, id: &str) {
    let label = format!(
        "session-{}",
        id.replace(|c: char| !c.is_ascii_alphanumeric(), "-")
    );
    if let Some(window) = app.get_webview_window(&label) {
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }
    let url = format!("index.html?session={id}");
    let _ = WebviewWindowBuilder::new(app, &label, WebviewUrl::App(url.into()))
        .title("AI SSH Session")
        .inner_size(1040.0, 720.0)
        .min_inner_size(720.0, 480.0)
        .build();
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            let show = MenuItem::with_id(app, "history", "Session History", true, None::<&str>)?;
            let config =
                MenuItem::with_id(app, "config", "Open Config Directory", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit Menubar", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &config, &quit])?;
            let mut tray = TrayIconBuilder::with_id("aissh-tray")
                .tooltip("AI SSH - host verification disabled")
                .menu(&menu);
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            tray.on_menu_event(|app, event| {
                let id = event.id().as_ref();
                if let Some(session_id) = id.strip_prefix("session:") {
                    open_session(app, session_id)
                } else if id == "history" {
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.show();
                        let _ = window.set_focus();
                    }
                } else if id == "config" {
                    if let Some(state) = app.try_state::<AppState>() {
                        let _ = std::process::Command::new("open")
                            .arg(&state.paths.root)
                            .spawn();
                    }
                } else if id == "quit" {
                    app.exit(0)
                }
            })
            .build(app)?;
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
                loop {
                    ticker.tick().await;
                    let Some(state) = handle.try_state::<AppState>() else {
                        continue;
                    };
                    let Ok(ResponseData::Sessions(sessions)) = state
                        .call(Request::SessionsList {
                            include_history: false,
                        })
                        .await
                    else {
                        continue;
                    };
                    let mut items = Vec::new();
                    for session in sessions.iter().filter(|s| {
                        !matches!(
                            s.status,
                            aissh_protocol::SessionStatus::Closed
                                | aissh_protocol::SessionStatus::Failed
                                | aissh_protocol::SessionStatus::Interrupted
                                | aissh_protocol::SessionStatus::Disconnected
                        )
                    }) {
                        let elapsed = (chrono::Utc::now() - session.created_at)
                            .num_seconds()
                            .max(0);
                        let command = session.current_command.as_deref().unwrap_or("Ready");
                        let title = format!(
                            "{} - {} - {}s [{:?}]",
                            session.target_name, command, elapsed, session.status
                        );
                        if let Ok(item) = MenuItem::with_id(
                            &handle,
                            format!("session:{}", session.id),
                            title,
                            true,
                            None::<&str>,
                        ) {
                            items.push(item)
                        }
                    }
                    if let Ok(item) =
                        MenuItem::with_id(&handle, "history", "Session History", true, None::<&str>)
                    {
                        items.push(item)
                    }
                    if let Ok(item) = MenuItem::with_id(
                        &handle,
                        "config",
                        "Open Config Directory",
                        true,
                        None::<&str>,
                    ) {
                        items.push(item)
                    }
                    if let Ok(item) =
                        MenuItem::with_id(&handle, "quit", "Quit Menubar", true, None::<&str>)
                    {
                        items.push(item)
                    }
                    let refs: Vec<&dyn tauri::menu::IsMenuItem<_>> =
                        items.iter().map(|v| v as _).collect();
                    if let Ok(menu) = Menu::with_items(&handle, &refs) {
                        if let Some(tray) = handle.tray_by_id("aissh-tray") {
                            let _ = tray.set_menu(Some(menu));
                        }
                    }
                }
            });
            Ok(())
        })
        .manage(AppState {
            paths: Paths::discover().expect("home directory"),
            request_id: AtomicU64::new(1),
        })
        .invoke_handler(tauri::generate_handler![
            targets,
            test_target,
            sessions,
            session_status,
            session_events,
            reload_config,
            retention_days,
            set_retention_days,
            open_config
        ])
        .run(tauri::generate_context!())
        .expect("error while running AI SSH");
}
