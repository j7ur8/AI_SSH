use aissh_config::{Config, Paths};
use aissh_protocol::{
    ErrorPayload, PROTOCOL_VERSION, Request, RequestFrame, Response, ResponseData, read_frame,
    write_frame,
};
use aissh_session::{SessionManager, events_response};
use aissh_storage::Storage;
use anyhow::{Context, Result, bail};
use std::{os::unix::fs::PermissionsExt, path::Path, sync::Arc, time::Duration};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;
use tracing::{error, info, warn};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let paths = Paths::discover()?;
    Config::ensure_exists(&paths)?;
    let config = Config::load(&paths).with_context(|| {
        format!(
            "cannot load {} (copy config.example.toml and chmod 600)",
            paths.config.display()
        )
    })?;
    let listener = bind_listener(&paths.socket).await?;
    let storage = Arc::new(Storage::open(&paths.database, config.recording_limit_mib)?);
    storage.interrupt_unfinished()?;
    storage.cleanup(config.retention_days)?;
    let manager = SessionManager::new(config, paths.clone(), storage);
    std::fs::set_permissions(&paths.socket, std::fs::Permissions::from_mode(0o600))?;
    info!(socket=%paths.socket.display(),"aisshd is ready");
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let reaper = Arc::clone(&manager);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            reaper.reap_idle().await;
        }
    });
    loop {
        tokio::select! {
            changed = shutdown_rx.changed() => {
                if changed.is_ok() && *shutdown_rx.borrow() {
                    break;
                }
            }
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                match same_user(&stream) {
                    Ok(true) => {
                        let manager = Arc::clone(&manager);
                        let shutdown = shutdown_tx.clone();
                        tokio::spawn(async move {
                            if let Err(error) = serve_client(stream, manager, shutdown).await {
                                warn!(%error,"IPC client disconnected");
                            }
                        });
                    }
                    Ok(false) => warn!("rejected IPC connection from another UID"),
                    Err(error) => error!(%error,"cannot inspect IPC peer"),
                }
            }
        }
    }
    drop(listener);
    let _ = std::fs::remove_file(&paths.socket);
    info!("aisshd stopped by local request");
    Ok(())
}

async fn bind_listener(path: &Path) -> Result<UnixListener> {
    if path.exists() {
        match UnixStream::connect(path).await {
            Ok(_) => bail!("aisshd is already running at {}", path.display()),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                ) =>
            {
                std::fs::remove_file(path)
                    .with_context(|| format!("cannot remove stale socket {}", path.display()))?;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("cannot inspect existing socket {}", path.display()));
            }
        }
    }
    UnixListener::bind(path)
        .with_context(|| format!("cannot bind daemon socket {}", path.display()))
}

fn same_user(stream: &UnixStream) -> Result<bool> {
    let peer = stream.peer_cred()?;
    Ok(peer.uid() == unsafe { libc::geteuid() })
}

async fn serve_client(
    mut stream: UnixStream,
    manager: Arc<SessionManager>,
    shutdown: watch::Sender<bool>,
) -> Result<()> {
    let mut client_name = String::from("unknown");
    loop {
        let frame: RequestFrame = match read_frame(&mut stream).await {
            Ok(v) => v,
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let mut shutdown_requested = false;
        let result = match frame.request {
            Request::Handshake {
                protocol_version,
                client_name: name,
            } => {
                if protocol_version != PROTOCOL_VERSION {
                    Err(ErrorPayload::new(
                        "PROTOCOL_MISMATCH",
                        format!(
                            "daemon uses protocol {PROTOCOL_VERSION}, client requested {protocol_version}"
                        ),
                    ))
                } else {
                    client_name = name;
                    Ok(ResponseData::Handshake {
                        protocol_version: PROTOCOL_VERSION,
                        daemon_version: env!("CARGO_PKG_VERSION").into(),
                    })
                }
            }
            Request::TargetsList => Ok(ResponseData::Targets(manager.targets().await)),
            Request::SessionsList { include_history } => manager
                .sessions(include_history)
                .await
                .map(ResponseData::Sessions),
            Request::SessionCreate { target_id, purpose } => manager
                .create(&target_id, &purpose, &client_name)
                .await
                .map(ResponseData::Session),
            Request::SessionStatus { session_id } => {
                manager.status(&session_id).await.map(ResponseData::Session)
            }
            Request::SessionClose { session_id } => {
                manager.close(&session_id).await.map(|_| ResponseData::Ack)
            }
            Request::ExecStart {
                session_id,
                command,
                cwd,
                env,
                timeout_seconds,
            } => manager
                .exec_start(&session_id, &command, cwd.as_deref(), env, timeout_seconds)
                .await
                .map(ResponseData::Command),
            Request::ExecBackground {
                session_id,
                command,
                cwd,
                env,
                timeout_seconds,
            } => manager
                .exec_background(&session_id, &command, cwd.as_deref(), env, timeout_seconds)
                .await
                .map(ResponseData::Command),
            Request::CommandPoll {
                command_id,
                after_sequence,
                max_bytes,
                wait_seconds,
            } => manager
                .command_poll(&command_id, after_sequence, max_bytes, wait_seconds)
                .await
                .map(|page| {
                    // `command` already carries the primary entry in full; a
                    // second copy in `commands` would re-send the same object
                    // (including the command text) on every single poll.
                    events_response(
                        Some(page.command),
                        Vec::new(),
                        page.events,
                        after_sequence,
                        page.more,
                        page.warnings,
                        page.progress,
                    )
                }),
            Request::CommandCancel { command_id } => manager
                .command_cancel(&command_id)
                .await
                .map(|_| ResponseData::Ack),
            Request::CommandsList {
                session_id,
                include_finished,
                limit,
            } => manager
                .commands(session_id.as_deref(), include_finished, limit)
                .await
                .map(ResponseData::Commands),
            Request::ShellOpen {
                session_id,
                cols,
                rows,
                term,
            } => manager
                .shell_open(&session_id, cols, rows, &term)
                .await
                .map(ResponseData::Session),
            Request::ShellWrite { session_id, data } => manager
                .shell_write(&session_id, &data)
                .await
                .map(|_| ResponseData::Ack),
            Request::ShellRead {
                session_id,
                after_sequence,
                max_bytes,
                wait_seconds,
            } => manager
                .shell_read(&session_id, after_sequence, max_bytes, wait_seconds)
                .await
                .map(|page| {
                    events_response(
                        None,
                        page.commands,
                        page.events,
                        after_sequence,
                        page.more,
                        Vec::new(),
                        page.progress,
                    )
                }),
            Request::ShellResize {
                session_id,
                cols,
                rows,
            } => manager
                .shell_resize(&session_id, cols, rows)
                .await
                .map(|_| ResponseData::Ack),
            Request::ShellClose { session_id } => manager
                .shell_close(&session_id)
                .await
                .map(|_| ResponseData::Ack),
            Request::FileStat {
                session_id,
                remote_path,
                hash,
            } => manager
                .file_stat(&session_id, &remote_path, hash)
                .await
                .map(ResponseData::FileStat),
            Request::FileRead {
                session_id,
                remote_path,
                max_bytes,
                sudo,
            } => manager
                .file_read(&session_id, &remote_path, max_bytes, sudo)
                .await
                .map(ResponseData::FileContent),
            Request::FileWrite {
                session_id,
                remote_path,
                data,
                append,
                create_dirs,
                mode,
                sudo,
                if_changed,
            } => manager
                .file_write(
                    &session_id,
                    &remote_path,
                    &data,
                    append,
                    create_dirs,
                    mode,
                    sudo,
                    if_changed,
                )
                .await
                .map(ResponseData::FileOutcome),
            Request::FileUpload {
                session_id,
                local_path,
                remote_path,
                create_dirs,
                mode,
                verify,
            } => manager
                .file_upload(
                    &session_id,
                    &local_path,
                    &remote_path,
                    create_dirs,
                    mode,
                    verify,
                )
                .await
                .map(ResponseData::FileOutcome),
            Request::FileDownload {
                session_id,
                remote_path,
                local_path,
                overwrite,
                create_dirs,
                verify,
            } => manager
                .file_download(
                    &session_id,
                    &remote_path,
                    &local_path,
                    overwrite,
                    create_dirs,
                    verify,
                )
                .await
                .map(ResponseData::FileOutcome),
            Request::FileMkdir {
                session_id,
                remote_path,
                parents,
                mode,
            } => manager
                .file_mkdir(&session_id, &remote_path, parents, mode)
                .await
                .map(|_| ResponseData::Ack),
            Request::ReloadConfig => manager.reload_config().await.map(|_| ResponseData::Ack),
            Request::DaemonShutdown => {
                shutdown_requested = true;
                Ok(ResponseData::Ack)
            }
        };
        write_frame(
            &mut stream,
            &Response {
                request_id: frame.request_id,
                result,
            },
        )
        .await?;
        if shutdown_requested {
            let _ = shutdown.send(true);
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[tokio::test]
    async fn refuses_to_replace_a_live_daemon_socket() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("aisshd-live-socket-{suffix}"));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("aisshd.sock");
        let listener = UnixListener::bind(&path).unwrap();

        let error = bind_listener(&path).await.unwrap_err();
        assert!(error.to_string().contains("already running"));

        drop(listener);
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&directory).unwrap();
    }
}
