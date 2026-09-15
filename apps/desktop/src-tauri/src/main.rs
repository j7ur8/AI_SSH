use aissh_config::{Config, Paths};
use aissh_protocol::{
    PROTOCOL_VERSION, Request, RequestFrame, Response, ResponseData, read_frame, write_frame,
};
use anyhow::{Context, Result as AnyResult, bail};
use std::{
    collections::HashMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::Duration,
};
use tauri::{
    Emitter, Manager, WebviewUrl, WebviewWindowBuilder,
    menu::{IconMenuItem, Menu, NativeIcon, PredefinedMenuItem},
    tray::TrayIconBuilder,
};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tokio::net::UnixStream;

mod updates;

struct AppState {
    paths: Paths,
    request_id: AtomicU64,
    exit_in_progress: AtomicBool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DaemonPromptAction {
    Start,
    Exit,
}

fn daemon_prompt_action(approved: bool) -> DaemonPromptAction {
    if approved {
        DaemonPromptAction::Start
    } else {
        DaemonPromptAction::Exit
    }
}

fn dispatch_daemon_prompt(approved: bool, start: impl FnOnce(), exit: impl FnOnce()) {
    match daemon_prompt_action(approved) {
        DaemonPromptAction::Start => start(),
        DaemonPromptAction::Exit => exit(),
    }
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
            // The observer polls on a timer; it never blocks the daemon.
            wait_seconds: 0,
        })
        .await
}
#[tauri::command]
async fn reload_config(state: tauri::State<'_, AppState>) -> Result<ResponseData, String> {
    state.call(Request::ReloadConfig).await
}
#[tauri::command]
fn config_get(state: tauri::State<'_, AppState>) -> Result<Config, String> {
    Config::load(&state.paths).map_err(|e| e.to_string())
}

fn set_autostart(app: &tauri::AppHandle, enabled: bool) -> Result<(), String> {
    let autolaunch = app.autolaunch();
    let is_enabled = autolaunch.is_enabled().map_err(|e| e.to_string())?;
    if is_enabled == enabled {
        return Ok(());
    }
    if enabled {
        autolaunch.enable()
    } else {
        autolaunch.disable()
    }
    .map_err(|e| e.to_string())
}

#[tauri::command]
async fn config_save(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    config: Config,
) -> Result<ResponseData, String> {
    let previous = Config::load(&state.paths).map_err(|e| e.to_string())?;
    config
        .save_atomic(&state.paths)
        .map_err(|e| e.to_string())?;
    if let Err(error) = set_autostart(&app, config.launch_at_login) {
        let _ = previous.save_atomic(&state.paths);
        let _ = set_autostart(&app, previous.launch_at_login);
        return Err(format!("cannot update launch at login: {error}"));
    }
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
#[tauri::command]
fn open_keys(state: tauri::State<'_, AppState>) -> Result<(), String> {
    std::process::Command::new("open")
        .arg(&state.paths.keys)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

async fn daemon_ready(paths: &Paths) -> bool {
    let probe = async {
        let mut stream = UnixStream::connect(&paths.socket).await.ok()?;
        write_frame(
            &mut stream,
            &RequestFrame {
                request_id: 1,
                request: Request::Handshake {
                    protocol_version: PROTOCOL_VERSION,
                    client_name: "AI SSH startup probe".into(),
                },
            },
        )
        .await
        .ok()?;
        let response: Response = read_frame(&mut stream).await.ok()?;
        response.result.ok()?;
        Some(())
    };
    tokio::time::timeout(Duration::from_secs(1), probe)
        .await
        .ok()
        .flatten()
        .is_some()
}

async fn wait_for_daemon(paths: &Paths) -> bool {
    for _ in 0..50 {
        if daemon_ready(paths).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// Directory names a packaged app may keep its helpers in.
fn helper_directories(app: &tauri::App) -> Vec<PathBuf> {
    let mut directories = Vec::new();
    if let Ok(resources) = app.path().resource_dir() {
        directories.push(resources.join("binaries"));
    }
    directories.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("binaries"));
    directories
}

/// Architectures this process could be running as, native first.
///
/// A universal binary is two slices with independent compilations, so each slice
/// reports its own architecture and can pick the matching helper.
fn runtime_arches() -> &'static [&'static str] {
    if cfg!(target_arch = "aarch64") {
        &["aarch64", "x86_64"]
    } else {
        &["x86_64", "aarch64"]
    }
}

/// Candidate helper file names, most specific first.
///
/// A single-architecture build stages exactly the triple the Tauri CLI compiles
/// into this binary. A universal build stages one helper per architecture, and a
/// lipo'd helper is accepted too, so the packaged name is matched in that order
/// before falling back to the other architecture.
fn helper_candidates(name: &str) -> Vec<String> {
    let mut candidates = vec![
        format!("{name}-{}", env!("TAURI_ENV_TARGET_TRIPLE")),
        format!("{name}-universal-apple-darwin"),
    ];
    for arch in runtime_arches() {
        let candidate = format!("{name}-{arch}-apple-darwin");
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }
    candidates
}

fn bundled_helper(app: &tauri::App, name: &str) -> Result<PathBuf, String> {
    let directories = helper_directories(app);
    let candidates = helper_candidates(name);
    for directory in &directories {
        for candidate in &candidates {
            let path = directory.join(candidate);
            if path.is_file() {
                return Ok(path);
            }
        }
    }
    Err(format!(
        "bundled helper {name} was not found; looked for [{}] in [{}]",
        candidates.join(", "),
        directories
            .iter()
            .map(|directory| directory.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

fn install_helper(source: &Path, destination: &Path) -> AnyResult<()> {
    if destination.exists() && fs::read(source)? == fs::read(destination)? {
        fs::set_permissions(destination, fs::Permissions::from_mode(0o700))?;
        return Ok(());
    }
    let temp = destination.with_extension("installing");
    fs::copy(source, &temp).with_context(|| {
        format!(
            "cannot copy helper {} to {}",
            source.display(),
            temp.display()
        )
    })?;
    fs::set_permissions(&temp, fs::Permissions::from_mode(0o700))?;
    fs::rename(&temp, destination)?;
    Ok(())
}

fn install_bundled_helpers(app: &tauri::App, paths: &Paths) -> AnyResult<()> {
    for name in ["aisshd", "aissh-mcp"] {
        let destination = paths.bin.join(name);
        match bundled_helper(app, name) {
            Ok(source) => install_helper(&source, &destination)?,
            // An older install keeps working when a build ships without helpers.
            Err(reason) => {
                if !destination.exists() {
                    bail!("{reason}");
                }
            }
        }
    }
    Ok(())
}

fn start_daemon(paths: &Paths) -> AnyResult<()> {
    let executable = paths.bin.join("aisshd");
    if !executable.exists() {
        bail!("daemon executable is missing at {}", executable.display());
    }
    let stdout = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.data.join("aisshd.stdout.log"))?;
    let stderr = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.data.join("aisshd.stderr.log"))?;
    Command::new(executable)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .context("cannot launch aisshd")?;
    Ok(())
}

fn show_startup_error(app: tauri::AppHandle, message: impl Into<String>) {
    let exit_handle = app.clone();
    app.dialog()
        .message(message)
        .title("AI SSH 无法启动")
        .kind(MessageDialogKind::Error)
        .buttons(MessageDialogButtons::Ok)
        .show(move |_| exit_handle.exit(1));
}

fn show_main_window(app: &tauri::AppHandle) -> AnyResult<()> {
    #[cfg(target_os = "macos")]
    app.set_activation_policy(tauri::ActivationPolicy::Regular)?;

    let window = if let Some(window) = app.get_webview_window("main") {
        window
    } else {
        WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
            .title("AI SSH Sessions")
            .inner_size(1040.0, 720.0)
            .min_inner_size(720.0, 480.0)
            .build()?
    };
    window.unminimize()?;
    window.show()?;
    window.set_focus()?;
    Ok(())
}

fn show_session_in_main(app: &tauri::AppHandle, session_id: &str) -> AnyResult<()> {
    show_main_window(app)?;
    app.emit_to("main", "select-session", session_id)?;
    Ok(())
}

fn session_native_icon(status: &aissh_protocol::SessionStatus) -> NativeIcon {
    use aissh_protocol::SessionStatus;
    match status {
        SessionStatus::Ready | SessionStatus::Idle => NativeIcon::StatusAvailable,
        SessionStatus::Connecting | SessionStatus::ExecRunning | SessionStatus::PtyOpen => {
            NativeIcon::StatusPartiallyAvailable
        }
        SessionStatus::Disconnected
        | SessionStatus::Interrupted
        | SessionStatus::Closed
        | SessionStatus::Failed => NativeIcon::StatusUnavailable,
    }
}

fn tray_action_menu(app: &tauri::AppHandle) -> AnyResult<Menu<tauri::Wry>> {
    let menu = Menu::new(app)?;
    let show = IconMenuItem::with_id_and_native_icon(
        app,
        "history",
        "Open AI SSH",
        true,
        Some(NativeIcon::Home),
        None::<&str>,
    )?;
    let separator = PredefinedMenuItem::separator(app)?;
    let check_updates = IconMenuItem::with_id_and_native_icon(
        app,
        "check-updates",
        "Check for Updates…",
        true,
        Some(NativeIcon::Refresh),
        None::<&str>,
    )?;
    let quit = IconMenuItem::with_id_and_native_icon(
        app,
        "quit",
        "Quit AI SSH",
        true,
        Some(NativeIcon::StopProgress),
        Some("Cmd+Q"),
    )?;
    menu.append(&show)?;
    menu.append(&separator)?;
    menu.append(&check_updates)?;
    menu.append(&quit)?;
    Ok(menu)
}

fn initialize_ui(app: &tauri::AppHandle, paths: &Paths, created_config: bool) -> AnyResult<()> {
    let menu = tray_action_menu(app)?;
    let mut tray = TrayIconBuilder::with_id("aissh-tray")
        .tooltip("AI SSH - host verification disabled")
        .menu(&menu);
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.on_menu_event(|app, event| {
        let id = event.id().as_ref();
        if let Some(session_id) = id.strip_prefix("session:") {
            let _ = show_session_in_main(app, session_id);
        } else if id == "history" {
            let _ = show_main_window(app);
        } else if id == "check-updates" {
            updates::spawn_manual_check(app.clone());
        } else if id == "quit" {
            app.exit(0)
        }
    })
    .build(app)?;

    let handle = app.clone();
    let live_menu = menu.clone();
    tauri::async_runtime::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        let mut displayed_ids = Vec::<String>::new();
        let mut session_items = HashMap::<String, IconMenuItem<tauri::Wry>>::new();
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
            let active: Vec<_> = sessions
                .iter()
                .filter(|session| {
                    !matches!(
                        session.status,
                        aissh_protocol::SessionStatus::Closed
                            | aissh_protocol::SessionStatus::Failed
                            | aissh_protocol::SessionStatus::Interrupted
                            | aissh_protocol::SessionStatus::Disconnected
                    )
                })
                .collect();
            let active_ids: Vec<_> = active.iter().map(|session| session.id.clone()).collect();

            if active_ids != displayed_ids {
                for item in session_items.values() {
                    let _ = live_menu.remove(item);
                }
                session_items.clear();
                for (index, session) in active.iter().enumerate() {
                    let item = match IconMenuItem::with_id_and_native_icon(
                        &handle,
                        format!("session:{}", session.id),
                        session.target_name.clone(),
                        true,
                        Some(session_native_icon(&session.status)),
                        None::<&str>,
                    ) {
                        Ok(item) => item,
                        Err(_) => continue,
                    };
                    if live_menu.insert(&item, index).is_ok() {
                        session_items.insert(session.id.clone(), item);
                    }
                }
                displayed_ids = active_ids;
            }

            for session in active {
                let elapsed = (chrono::Utc::now() - session.created_at)
                    .num_seconds()
                    .max(0);
                let command = session.current_command.as_deref().unwrap_or("Ready");
                let title = format!(
                    "{} - {} - {}s [{:?}]",
                    session.target_name, command, elapsed, session.status
                );
                if let Some(item) = session_items.get(&session.id) {
                    let _ = item.set_text(title);
                    let _ = item.set_native_icon(Some(session_native_icon(&session.status)));
                }
            }
        }
    });

    let has_no_targets = Config::load(paths)
        .map(|config| config.targets.is_empty())
        .unwrap_or(false);
    if created_config || has_no_targets {
        show_main_window(app)?;
    }
    // Spawned from here because this runs once per launch on every path, after
    // the daemon question has been settled, and the check itself is delayed.
    updates::spawn_startup_check(app.clone());
    Ok(())
}

fn finish_startup(app: tauri::AppHandle, paths: Paths, created_config: bool) {
    let ui_handle = app.clone();
    let error_handle = app.clone();
    if let Err(error) = app.run_on_main_thread(move || {
        if let Err(error) = initialize_ui(&ui_handle, &paths, created_config) {
            show_startup_error(error_handle, format!("无法创建 Menubar：\n{error}"));
        }
    }) {
        show_startup_error(app, format!("无法进入应用主线程：\n{error}"));
    }
}

fn request_daemon_start(app: tauri::AppHandle, paths: Paths, created_config: bool) {
    let decision_handle = app.clone();
    app.dialog()
        .message("AI SSH 后台服务尚未运行。是否授权启动本地 aisshd 服务？")
        .title("启动 AI SSH 后台服务")
        .kind(MessageDialogKind::Info)
        .buttons(MessageDialogButtons::OkCancelCustom(
            "启动".into(),
            "退出".into(),
        ))
        .show(move |approved| {
            let exit_handle = decision_handle.clone();
            dispatch_daemon_prompt(
                approved,
                move || {
                    let startup_handle = decision_handle.clone();
                    tauri::async_runtime::spawn(async move {
                        if !daemon_ready(&paths).await {
                            if let Err(error) = start_daemon(&paths) {
                                show_startup_error(
                                    startup_handle,
                                    format!("后台服务启动失败：\n{error}"),
                                );
                                return;
                            }
                            if !wait_for_daemon(&paths).await {
                                show_startup_error(
                                    startup_handle,
                                    format!(
                                        "后台服务未能在 5 秒内就绪。\n日志：{}",
                                        paths.data.join("aisshd.stderr.log").display()
                                    ),
                                );
                                return;
                            }
                        }
                        finish_startup(startup_handle, paths, created_config);
                    });
                },
                move || {
                    exit_handle.exit(0);
                },
            );
        });
}

fn main() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
                #[cfg(target_os = "macos")]
                if window.label() == "main" {
                    let _ = window
                        .app_handle()
                        .set_activation_policy(tauri::ActivationPolicy::Accessory);
                }
            }
        })
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let paths = Paths::discover()?;
            let created_config = match Config::ensure_exists(&paths) {
                Ok(created) => created,
                Err(error) => {
                    show_startup_error(
                        app.handle().clone(),
                        format!("无法创建配置文件：\n{error}"),
                    );
                    return Ok(());
                }
            };
            let config = match Config::load(&paths) {
                Ok(config) => config,
                Err(error) => {
                    show_startup_error(
                        app.handle().clone(),
                        format!("无法读取配置文件：\n{error}"),
                    );
                    return Ok(());
                }
            };
            if let Err(error) = set_autostart(app.handle(), config.launch_at_login) {
                show_startup_error(
                    app.handle().clone(),
                    format!("无法更新开机启动设置：\n{error}"),
                );
                return Ok(());
            }
            if let Err(error) = install_bundled_helpers(app, &paths) {
                show_startup_error(app.handle().clone(), format!("无法安装后台组件：\n{error}"));
                return Ok(());
            }

            if !tauri::async_runtime::block_on(daemon_ready(&paths)) {
                request_daemon_start(app.handle().clone(), paths, created_config);
                return Ok(());
            }
            initialize_ui(app.handle(), &paths, created_config)?;
            Ok(())
        })
        .manage(AppState {
            paths: Paths::discover().expect("home directory"),
            request_id: AtomicU64::new(1),
            exit_in_progress: AtomicBool::new(false),
        })
        .invoke_handler(tauri::generate_handler![
            targets,
            test_target,
            sessions,
            session_status,
            session_events,
            reload_config,
            config_get,
            config_save,
            retention_days,
            set_retention_days,
            open_config,
            open_keys
        ])
        .build(tauri::generate_context!())
        .expect("error while building AI SSH");

    app.run(|app, event| {
        if let tauri::RunEvent::ExitRequested { api, .. } = event {
            let state = app.state::<AppState>();
            let stop_daemon = Config::load(&state.paths)
                .map(|config| config.quit_daemon_on_app_exit)
                .unwrap_or(false);
            if stop_daemon
                && state
                    .exit_in_progress
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                api.prevent_exit();
                let handle = app.clone();
                tauri::async_runtime::spawn(async move {
                    if let Some(state) = handle.try_state::<AppState>() {
                        let _ = tokio::time::timeout(
                            Duration::from_secs(2),
                            state.call(Request::DaemonShutdown),
                        )
                        .await;
                    }
                    handle.exit(0);
                });
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_lookup_prefers_the_native_architecture() {
        let candidates = helper_candidates("aisshd");
        let native = runtime_arches()[0];
        let foreign = runtime_arches()[1];

        assert_eq!(
            candidates[0],
            format!("aisshd-{}", env!("TAURI_ENV_TARGET_TRIPLE")),
            "a single-architecture build stages exactly the compiled triple"
        );
        assert!(
            candidates.contains(&"aisshd-universal-apple-darwin".to_string()),
            "a lipo'd helper must still be found: {candidates:?}"
        );

        let native_candidate = format!("aisshd-{native}-apple-darwin");
        let foreign_candidate = format!("aisshd-{foreign}-apple-darwin");
        let native_index = candidates.iter().position(|item| item == &native_candidate);
        let foreign_index = candidates
            .iter()
            .position(|item| item == &foreign_candidate);
        assert!(
            native_index < foreign_index,
            "a universal app carrying both helpers must pick the one matching this process: {candidates:?}"
        );
    }

    #[test]
    fn helper_candidates_never_repeat_a_name() {
        for name in ["aisshd", "aissh-mcp"] {
            let candidates = helper_candidates(name);
            let mut unique = candidates.clone();
            unique.sort();
            unique.dedup();
            assert_eq!(
                unique.len(),
                candidates.len(),
                "duplicate candidates waste lookups: {candidates:?}"
            );
            assert!(
                candidates.iter().all(|item| item.starts_with(name)),
                "candidates must stay scoped to one helper: {candidates:?}"
            );
        }
    }

    #[test]
    fn daemon_prompt_only_starts_after_approval() {
        assert_eq!(daemon_prompt_action(true), DaemonPromptAction::Start);
        assert_eq!(daemon_prompt_action(false), DaemonPromptAction::Exit);
    }

    #[test]
    fn denied_daemon_prompt_dispatches_exit_only() {
        let started = std::cell::Cell::new(false);
        let exited = std::cell::Cell::new(false);
        dispatch_daemon_prompt(false, || started.set(true), || exited.set(true));
        assert!(!started.get());
        assert!(exited.get());
    }

    #[test]
    fn session_statuses_map_to_native_menu_icons() {
        use aissh_protocol::SessionStatus;
        assert_eq!(
            session_native_icon(&SessionStatus::Ready),
            NativeIcon::StatusAvailable
        );
        assert_eq!(
            session_native_icon(&SessionStatus::ExecRunning),
            NativeIcon::StatusPartiallyAvailable
        );
        assert_eq!(
            session_native_icon(&SessionStatus::Failed),
            NativeIcon::StatusUnavailable
        );
    }
}
