// A release build is a Windows GUI application, so launching it from a shortcut
// does not open a console window beside the tray icon. A debug build keeps the
// console, which is where its log output and panics are readable.
#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]

use aissh_config::{Config, Paths};
use aissh_ipc::Stream;
use aissh_protocol::{
    PROTOCOL_VERSION, Request, RequestFrame, Response, ResponseData, read_frame, write_frame,
};
use anyhow::{Context, Result as AnyResult, bail};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::Duration,
};
#[cfg(not(target_os = "macos"))]
use tauri::menu::MenuItem;
#[cfg(target_os = "macos")]
use tauri::menu::{IconMenuItem, NativeIcon};
use tauri::{
    Emitter, Manager, WebviewUrl, WebviewWindowBuilder,
    menu::{Menu, PredefinedMenuItem},
    tray::TrayIconBuilder,
};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

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
        let mut stream = Stream::connect(&self.paths.endpoint())
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
/// Opens a directory in the platform's file manager.
fn open_in_file_manager(path: &Path) -> Result<(), String> {
    #[cfg(windows)]
    let program = "explorer";
    #[cfg(not(windows))]
    let program = "open";

    Command::new(program)
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn open_config(state: tauri::State<'_, AppState>) -> Result<(), String> {
    open_in_file_manager(&state.paths.root)
}
#[tauri::command]
fn open_keys(state: tauri::State<'_, AppState>) -> Result<(), String> {
    open_in_file_manager(&state.paths.keys)
}

async fn daemon_ready(paths: &Paths) -> bool {
    let probe = async {
        let mut stream = Stream::connect(&paths.endpoint()).await.ok()?;
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

/// The suffix a program carries on this platform. Windows resolves an executable
/// by its extension, so both the staged and the installed helper need one.
const EXECUTABLE_SUFFIX: &str = if cfg!(windows) { ".exe" } else { "" };

/// The name a helper is installed under inside the configuration's `bin`
/// directory.
fn installed_helper_name(name: &str) -> String {
    format!("{name}{EXECUTABLE_SUFFIX}")
}

/// Architectures this process could be running as, native first.
///
/// A universal binary is two slices with independent compilations, so each slice
/// reports its own architecture and can pick the matching helper. Only macOS
/// ships one.
#[cfg(target_os = "macos")]
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
/// into this binary. A universal macOS build additionally stages one helper per
/// architecture, and a lipo'd helper is accepted too, so the packaged name is
/// matched in that order before falling back to the other architecture.
fn helper_candidates(name: &str) -> Vec<String> {
    let native = format!(
        "{name}-{}{EXECUTABLE_SUFFIX}",
        env!("TAURI_ENV_TARGET_TRIPLE")
    );
    #[cfg(target_os = "macos")]
    {
        let mut candidates = vec![native, format!("{name}-universal-apple-darwin")];
        for arch in runtime_arches() {
            let candidate = format!("{name}-{arch}-apple-darwin");
            if !candidates.contains(&candidate) {
                candidates.push(candidate);
            }
        }
        candidates
    }
    #[cfg(not(target_os = "macos"))]
    {
        vec![native]
    }
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

/// Gives an installed helper the mode Unix needs to execute it. Windows has no
/// mode, and the helpers already live in a directory `Paths::ensure` restricted
/// to the owner.
#[cfg(unix)]
fn restrict_helper(path: &Path) -> AnyResult<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_helper(_path: &Path) -> AnyResult<()> {
    Ok(())
}

/// Moves `temp` into place over `destination`.
#[cfg(not(windows))]
fn replace_helper(temp: &Path, destination: &Path) -> AnyResult<()> {
    // Renaming over the destination is atomic, so a reader never sees a partial
    // helper.
    fs::rename(temp, destination)?;
    Ok(())
}

/// Windows refuses to overwrite a program that is currently running, but it does
/// allow that program to be renamed out of the way, so the old helper is parked
/// under a name of its own and removed once whichever process held it exits.
#[cfg(windows)]
fn replace_helper(temp: &Path, destination: &Path) -> AnyResult<()> {
    let displaced = displaced_path(destination);
    if destination.exists() {
        fs::rename(destination, &displaced).with_context(|| {
            format!(
                "cannot move the previous helper {} aside",
                destination.display()
            )
        })?;
    }
    fs::rename(temp, destination)?;
    let _ = fs::remove_file(&displaced);
    Ok(())
}

/// The name a superseded helper is parked under.
///
/// It carries the process id and a timestamp so that two replacements can never
/// want the same name. A parked file that a running daemon still holds cannot be
/// deleted, and moving the next helper onto a name that is already taken would
/// fail the install instead of the delete.
#[cfg(windows)]
fn displaced_path(destination: &Path) -> PathBuf {
    let name = destination
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    destination.with_file_name(format!("{name}.replaced-{}-{stamp}", std::process::id()))
}

/// Removes the helpers earlier replacements parked, which could not be deleted at
/// the time because a running process still held them. Retried on every launch so
/// the directory converges instead of accumulating them.
#[cfg(windows)]
fn sweep_displaced(destination: &Path) {
    let (Some(directory), Some(name)) = (destination.parent(), destination.file_name()) else {
        return;
    };
    let prefix = format!("{}.replaced-", name.to_string_lossy());
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[cfg(not(windows))]
fn sweep_displaced(_destination: &Path) {}

fn install_helper(source: &Path, destination: &Path) -> AnyResult<()> {
    // A helper a previous run parked can only be removed once the process that
    // held it has exited, so the sweep runs on every launch rather than only when
    // the binary changes.
    sweep_displaced(destination);
    if destination.exists() && fs::read(source)? == fs::read(destination)? {
        restrict_helper(destination)?;
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
    restrict_helper(&temp)?;
    replace_helper(&temp, destination)?;
    Ok(())
}

fn install_bundled_helpers(app: &tauri::App, paths: &Paths) -> AnyResult<()> {
    for name in ["aisshd", "aissh-mcp"] {
        let destination = paths.bin.join(installed_helper_name(name));
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
    let executable = paths.bin.join(installed_helper_name("aisshd"));
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
    let mut command = Command::new(executable);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    // The app has no console of its own, so Windows would give this
    // console-subsystem child one: a black window on the desktop for a daemon
    // that reports through the log files opened above.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command.spawn().context("cannot launch aisshd")?;
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

/// A tray entry that can carry a glyph beside its label.
///
/// macOS draws a native icon next to the item. The Windows tray menu is text
/// only, so the same entry is a plain menu item there and a session's state stays
/// in its label.
#[cfg(target_os = "macos")]
type TrayEntry = IconMenuItem<tauri::Wry>;
#[cfg(not(target_os = "macos"))]
type TrayEntry = MenuItem<tauri::Wry>;

/// macOS names the quit accelerator with Command; other platforms use Control.
const QUIT_ACCELERATOR: &str = if cfg!(target_os = "macos") {
    "Cmd+Q"
} else {
    "Ctrl+Q"
};

/// The icon a fixed action entry carries, or nothing for a session entry, which
/// carries its connection state instead.
#[cfg(target_os = "macos")]
fn action_glyph(id: &str) -> Option<NativeIcon> {
    Some(match id {
        "history" => NativeIcon::Home,
        "check-updates" => NativeIcon::Refresh,
        "quit" => NativeIcon::StopProgress,
        _ => return None,
    })
}

#[cfg(target_os = "macos")]
fn status_glyph(status: Option<&aissh_protocol::SessionStatus>) -> Option<NativeIcon> {
    use aissh_protocol::SessionStatus;
    Some(match status? {
        SessionStatus::Ready | SessionStatus::Idle => NativeIcon::StatusAvailable,
        SessionStatus::Connecting | SessionStatus::ExecRunning | SessionStatus::PtyOpen => {
            NativeIcon::StatusPartiallyAvailable
        }
        SessionStatus::Disconnected
        | SessionStatus::Interrupted
        | SessionStatus::Closed
        | SessionStatus::Failed => NativeIcon::StatusUnavailable,
    })
}

fn tray_entry(
    app: &tauri::AppHandle,
    id: &str,
    text: &str,
    accelerator: Option<&str>,
    status: Option<&aissh_protocol::SessionStatus>,
) -> AnyResult<TrayEntry> {
    #[cfg(target_os = "macos")]
    {
        Ok(IconMenuItem::with_id_and_native_icon(
            app,
            id,
            text,
            true,
            action_glyph(id).or_else(|| status_glyph(status)),
            accelerator,
        )?)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = status;
        Ok(MenuItem::with_id(app, id, text, true, accelerator)?)
    }
}

/// Updates the glyph on an existing entry. Only macOS renders one.
fn set_tray_status(entry: &TrayEntry, status: Option<&aissh_protocol::SessionStatus>) {
    #[cfg(target_os = "macos")]
    {
        let _ = entry.set_native_icon(status_glyph(status));
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (entry, status);
    }
}

fn tray_action_menu(app: &tauri::AppHandle) -> AnyResult<Menu<tauri::Wry>> {
    let menu = Menu::new(app)?;
    let show = tray_entry(app, "history", "Open AI SSH", None, None)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let check_updates = tray_entry(app, "check-updates", "Check for Updates…", None, None)?;
    let quit = tray_entry(app, "quit", "Quit AI SSH", Some(QUIT_ACCELERATOR), None)?;
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
        let mut session_items = HashMap::<String, TrayEntry>::new();
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
                    let Ok(item) = tray_entry(
                        &handle,
                        &format!("session:{}", session.id),
                        &session.target_name,
                        None,
                        Some(&session.status),
                    ) else {
                        continue;
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
                    set_tray_status(item, Some(&session.status));
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

    /// Only a macOS build stages more than one helper, so only there can the
    /// ordering between them be observed.
    #[cfg(target_os = "macos")]
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

    #[cfg(target_os = "macos")]
    #[test]
    fn session_statuses_map_to_native_menu_icons() {
        use aissh_protocol::SessionStatus;
        assert_eq!(
            status_glyph(Some(&SessionStatus::Ready)),
            Some(NativeIcon::StatusAvailable)
        );
        assert_eq!(
            status_glyph(Some(&SessionStatus::ExecRunning)),
            Some(NativeIcon::StatusPartiallyAvailable)
        );
        assert_eq!(
            status_glyph(Some(&SessionStatus::Failed)),
            Some(NativeIcon::StatusUnavailable)
        );
    }

    /// Windows runs a program by name only when it carries the extension it
    /// resolves executables by.
    #[cfg(windows)]
    #[test]
    fn helpers_are_installed_under_a_name_windows_resolves() {
        for name in ["aisshd", "aissh-mcp"] {
            assert_eq!(installed_helper_name(name), format!("{name}.exe"));
            assert!(
                helper_candidates(name)
                    .iter()
                    .all(|candidate| candidate.ends_with(".exe")),
                "a staged helper must keep the extension it was built with"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn helpers_are_installed_without_an_extension() {
        assert_eq!(installed_helper_name("aisshd"), "aisshd");
        assert_eq!(installed_helper_name("aissh-mcp"), "aissh-mcp");
    }

    /// The parked name has to be unique, because a file a running daemon still
    /// holds cannot be deleted and a second replacement must not fail trying to
    /// take a name that is already in use.
    #[cfg(windows)]
    #[test]
    fn a_parked_helper_has_a_name_of_its_own() {
        let destination = Path::new(r"C:\config\bin\aisshd.exe");
        let parked = displaced_path(destination);

        assert_ne!(parked, destination);
        assert_eq!(parked.parent(), destination.parent());
        assert!(
            parked
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("aisshd.exe.replaced-"),
            "{}",
            parked.display()
        );
    }

    /// The sweep is what keeps parked helpers from accumulating across upgrades,
    /// and it has to leave everything else in the directory alone.
    #[cfg(windows)]
    #[test]
    fn a_launch_sweeps_the_helpers_earlier_launches_parked() {
        let directory = std::env::temp_dir().join(format!("aissh-sweep-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let destination = directory.join("aisshd.exe");
        fs::write(&destination, b"current").unwrap();
        let parked = directory.join("aisshd.exe.replaced-1-2");
        fs::write(&parked, b"superseded").unwrap();
        let other_helper = directory.join("aissh-mcp.exe.replaced-1-2");
        fs::write(&other_helper, b"another helper").unwrap();

        sweep_displaced(&destination);

        assert!(!parked.exists(), "a parked helper must be removed");
        assert!(destination.exists(), "the installed helper must survive");
        assert!(
            other_helper.exists(),
            "the sweep must only touch the helper it names"
        );
        fs::remove_dir_all(&directory).unwrap();
    }
}
