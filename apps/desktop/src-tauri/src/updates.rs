//! Automatic update checks.
//!
//! The updater downloads a release manifest from the endpoint in
//! `tauri.conf.json` and only accepts the archive it names if a minisign
//! signature over that archive matches the public key in the same config. Apple
//! code signing is a separate concern and is not required for this path, so an
//! unsigned build can still update itself as long as the signing key pair is the
//! one this repository publishes.
//!
//! The check is automatic; installing is not. Restarting under someone is worse
//! than being one version behind.

use std::time::Duration;
use tauri::AppHandle;
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tauri_plugin_updater::{Update, UpdaterExt};
use tokio::sync::oneshot;

/// Delay before the automatic check, so a cold start is not competing with a
/// network request and the menu bar appears immediately.
const STARTUP_DELAY: Duration = Duration::from_secs(8);

/// Which path asked for the check, which decides how the outcome is reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// The automatic check at launch.
    Startup,
    /// An explicit request from the menu bar.
    Manual,
}

/// Starts the automatic check. Returns immediately.
pub fn spawn_startup_check(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(STARTUP_DELAY).await;
        check(app, Origin::Startup).await;
    });
}

/// Runs a check because the user asked for one.
pub fn spawn_manual_check(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        check(app, Origin::Manual).await;
    });
}

/// Whether an outcome other than "an update is available" should be shown to the
/// user.
///
/// An automatic check stays quiet about being up to date and about failing,
/// otherwise every offline launch would open an error dialog nobody asked for.
/// Both are written to the log instead. An update being available is handled
/// before this point and always prompts.
fn should_report(origin: Origin) -> bool {
    match origin {
        Origin::Startup => false,
        Origin::Manual => true,
    }
}

async fn check(app: AppHandle, origin: Origin) {
    let updater = match app.updater() {
        Ok(updater) => updater,
        Err(error) => {
            report(&app, origin, format!("无法初始化更新检查：{error}"), true);
            return;
        }
    };

    match updater.check().await {
        Ok(Some(update)) => confirm(app, update).await,
        Ok(None) => report(
            &app,
            origin,
            format!("已是最新版本（{}）。", app.package_info().version),
            false,
        ),
        Err(error) => report(&app, origin, format!("检查更新失败：{error}"), true),
    }
}

/// Asks before downloading anything.
///
/// The decision arrives over a channel rather than being handled in the dialog
/// callback, so the `Update` stays owned by this task instead of crossing a
/// thread boundary. Re-checking after the prompt would also reopen the window
/// where the announced version changes between the question and the install.
async fn confirm(app: AppHandle, update: Update) {
    let version = update.version.clone();
    let current = update.current_version.clone();
    let notes = update
        .body
        .as_deref()
        .map(str::trim)
        .filter(|notes| !notes.is_empty())
        .map(|notes| format!("\n\n更新说明：\n{notes}"))
        .unwrap_or_default();

    let (sender, receiver) = oneshot::channel();
    app.dialog()
        .message(format!(
            "AI SSH {version} 已发布，当前版本为 {current}。\n\n现在下载并安装吗？安装完成后应用会重新启动。后台服务 aisshd 是独立进程，正在进行的 SSH 会话不会中断。{notes}"
        ))
        .title("发现新版本")
        .kind(MessageDialogKind::Info)
        .buttons(MessageDialogButtons::OkCancelCustom(
            "安装".into(),
            "稍后".into(),
        ))
        .show(move |approved| {
            let _ = sender.send(approved);
        });

    if receiver.await.unwrap_or(false) {
        install(app, update, version).await;
    }
}

async fn install(app: AppHandle, update: Update, version: String) {
    // The app has no window open at this point, so there is nowhere to draw
    // progress; the confirmation dialog below is the only feedback.
    match update.download_and_install(|_, _| {}, || {}).await {
        // `restart` replaces this process, which is what applies the new bundle.
        Ok(()) => app.restart(),
        Err(error) => report(
            &app,
            Origin::Manual,
            format!("更新 {version} 安装失败：{error}"),
            true,
        ),
    }
}

/// Writes the outcome to the log, and to a dialog only when the user asked.
fn report(app: &AppHandle, origin: Origin, message: String, failed: bool) {
    if !should_report(origin) {
        eprintln!("ai-ssh update check: {message}");
        return;
    }
    app.dialog()
        .message(message)
        .title(if failed {
            "检查更新失败"
        } else {
            "检查更新"
        })
        .kind(if failed {
            MessageDialogKind::Error
        } else {
            MessageDialogKind::Info
        })
        .buttons(MessageDialogButtons::Ok)
        .show(|_| {});
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_checks_never_interrupt() {
        assert!(
            !should_report(Origin::Startup),
            "an automatic check must not open a dialog for an up-to-date result or a failure"
        );
    }

    #[test]
    fn requested_checks_always_answer() {
        assert!(
            should_report(Origin::Manual),
            "a check the user asked for has to report what happened"
        );
    }
}
