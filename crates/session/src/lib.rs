use aissh_config::{Config, Paths};
use aissh_protocol::{
    CommandInfo, CommandStatus, ErrorPayload, ResponseData, SessionInfo, SessionStatus, StreamKind,
    TargetSummary, TerminalEvent,
};
use aissh_ssh::{ChannelEvent, PtyWriter, SshConnection, build_command};
use aissh_storage::Storage;
use chrono::Utc;
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

type ApiResult<T> = Result<T, ErrorPayload>;

pub struct SessionManager {
    config: RwLock<Arc<Config>>,
    paths: Paths,
    storage: Arc<Storage>,
    sessions: RwLock<HashMap<String, Arc<SessionRuntime>>>,
}

struct SessionRuntime {
    session_id: String,
    info: RwLock<SessionInfo>,
    connection: Mutex<Option<SshConnection>>,
    foreground: Mutex<Option<Foreground>>,
    background: Mutex<HashMap<String, CancellationToken>>,
    sequence: AtomicU64,
    recording_truncated: AtomicBool,
    last_activity: StdMutex<Instant>,
    live_events: StdMutex<VecDeque<TerminalEvent>>,
}

enum Foreground {
    Exec {
        command_id: String,
        cancel: CancellationToken,
    },
    Pty {
        writer: Arc<PtyWriter>,
    },
}

impl SessionManager {
    pub fn new(config: Config, paths: Paths, storage: Arc<Storage>) -> Arc<Self> {
        Arc::new(Self {
            config: RwLock::new(Arc::new(config)),
            paths,
            storage,
            sessions: RwLock::new(HashMap::new()),
        })
    }

    pub async fn targets(&self) -> Vec<TargetSummary> {
        self.config
            .read()
            .await
            .targets
            .iter()
            .map(|target| TargetSummary {
                id: target.id.clone(),
                name: target.name.clone(),
                host: target.host.clone(),
                port: target.port,
                username: target.username.clone(),
                host_verification: false,
            })
            .collect()
    }

    pub async fn sessions(&self, include_history: bool) -> ApiResult<Vec<SessionInfo>> {
        let active = self.sessions.read().await;
        let mut result = Vec::with_capacity(active.len());
        for runtime in active.values() {
            let mut info = runtime.info.read().await.clone();
            info.recording_truncated = runtime.recording_truncated.load(Ordering::Relaxed);
            result.push(info);
        }
        drop(active);
        if include_history {
            for item in self.storage.sessions().map_err(internal)? {
                if !result.iter().any(|active| active.id == item.id) {
                    result.push(item);
                }
            }
        }
        result.sort_by_key(|item| std::cmp::Reverse(item.created_at));
        Ok(result)
    }

    pub async fn create(
        self: &Arc<Self>,
        target_id: &str,
        purpose: &str,
        client_name: &str,
    ) -> ApiResult<SessionInfo> {
        let config = self.config.read().await.clone();
        let target = config
            .targets
            .iter()
            .find(|target| target.id == target_id)
            .cloned()
            .ok_or_else(|| {
                ErrorPayload::new(
                    "TARGET_NOT_FOUND",
                    format!("target {target_id:?} does not exist"),
                )
            })?;
        let now = Utc::now();
        let info = SessionInfo {
            id: Uuid::new_v4().to_string(),
            target_id: target.id.clone(),
            target_name: target.name.clone(),
            purpose: purpose.into(),
            client_name: client_name.into(),
            status: SessionStatus::Connecting,
            current_command: None,
            last_exit_code: None,
            recording_truncated: false,
            host_fingerprint: None,
            created_at: now,
            updated_at: now,
            closed_at: None,
        };
        let runtime = Arc::new(SessionRuntime {
            session_id: info.id.clone(),
            info: RwLock::new(info.clone()),
            connection: Mutex::new(None),
            foreground: Mutex::new(None),
            background: Mutex::new(HashMap::new()),
            sequence: AtomicU64::new(0),
            recording_truncated: AtomicBool::new(false),
            last_activity: StdMutex::new(Instant::now()),
            live_events: StdMutex::new(VecDeque::new()),
        });
        self.storage.upsert_session(&info).map_err(internal)?;
        self.sessions
            .write()
            .await
            .insert(info.id.clone(), Arc::clone(&runtime));
        match SshConnection::connect(
            &target,
            &self.paths,
            Duration::from_secs(config.connect_timeout_seconds),
            Duration::from_secs(config.keepalive_seconds),
        )
        .await
        {
            Ok(connection) => {
                let fingerprint = connection.fingerprint().to_owned();
                *runtime.connection.lock().await = Some(connection);
                let mut info = runtime.info.write().await;
                info.status = SessionStatus::Ready;
                info.host_fingerprint = Some(fingerprint);
                info.updated_at = Utc::now();
                self.storage.upsert_session(&info).map_err(internal)?;
                Ok(info.clone())
            }
            Err(error) => {
                let mut info = runtime.info.write().await;
                info.status = SessionStatus::Failed;
                info.updated_at = Utc::now();
                info.closed_at = Some(info.updated_at);
                self.storage.upsert_session(&info).map_err(internal)?;
                Err(classify_error(error))
            }
        }
    }

    pub async fn status(&self, session_id: &str) -> ApiResult<SessionInfo> {
        let runtime = self.sessions.read().await.get(session_id).cloned();
        if let Some(runtime) = runtime {
            let mut info = runtime.info.read().await.clone();
            info.recording_truncated = runtime.recording_truncated.load(Ordering::Relaxed);
            return Ok(info);
        }
        self.storage
            .session(session_id)
            .map_err(internal)?
            .ok_or_else(|| session_not_found(session_id))
    }

    pub async fn close(&self, session_id: &str) -> ApiResult<()> {
        let runtime = self.runtime(session_id).await?;
        if let Some(foreground) = runtime.foreground.lock().await.take() {
            match foreground {
                Foreground::Exec { cancel, .. } => cancel.cancel(),
                Foreground::Pty { writer } => {
                    let _ = writer.close().await;
                }
            }
        }
        for (_, cancel) in runtime.background.lock().await.drain() {
            cancel.cancel();
        }
        if let Some(connection) = runtime.connection.lock().await.take() {
            let _ = connection.disconnect().await;
        }
        let mut info = runtime.info.write().await;
        info.status = SessionStatus::Closed;
        info.current_command = None;
        info.updated_at = Utc::now();
        info.closed_at = Some(info.updated_at);
        self.storage.upsert_session(&info).map_err(internal)
    }

    pub async fn exec_start(
        self: &Arc<Self>,
        session_id: &str,
        command: &str,
        cwd: Option<&str>,
        env: BTreeMap<String, String>,
        timeout_seconds: Option<u64>,
    ) -> ApiResult<CommandInfo> {
        self.start_exec(session_id, command, cwd, env, timeout_seconds, false)
            .await
    }

    pub async fn exec_background(
        self: &Arc<Self>,
        session_id: &str,
        command: &str,
        cwd: Option<&str>,
        env: BTreeMap<String, String>,
        timeout_seconds: Option<u64>,
    ) -> ApiResult<CommandInfo> {
        self.start_exec(session_id, command, cwd, env, timeout_seconds, true)
            .await
    }

    async fn start_exec(
        self: &Arc<Self>,
        session_id: &str,
        command: &str,
        cwd: Option<&str>,
        env: BTreeMap<String, String>,
        timeout_seconds: Option<u64>,
        background: bool,
    ) -> ApiResult<CommandInfo> {
        if command.trim().is_empty() {
            return Err(ErrorPayload::new(
                "INVALID_ARGUMENT",
                "command must not be empty",
            ));
        }
        if clearly_interactive(command) {
            return Err(ErrorPayload::new(
                "INTERACTIVE_REQUIRED",
                "this command is explicitly interactive; open a PTY for this session",
            ));
        }
        let runtime = self.runtime(session_id).await?;
        let mut foreground = runtime.foreground.lock().await;
        if !background && foreground.is_some() {
            return Err(ErrorPayload::new(
                "SESSION_BUSY",
                "session already has a foreground exec or PTY",
            )
            .retryable());
        }
        let remote_command = build_command(command, cwd, &env).map_err(classify_error)?;
        let mut connection = runtime.connection.lock().await;
        let channel = connection
            .as_mut()
            .ok_or_else(|| {
                ErrorPayload::new("SESSION_DISCONNECTED", "SSH connection is not available")
            })?
            .open_exec(&remote_command)
            .await
            .map_err(classify_error)?;
        let now = Utc::now();
        let command_info = CommandInfo {
            id: Uuid::new_v4().to_string(),
            session_id: session_id.into(),
            command: command.into(),
            status: CommandStatus::Running,
            exit_code: None,
            started_at: now,
            finished_at: None,
            last_sequence: 0,
            recording_truncated: false,
        };
        self.storage
            .insert_command(&command_info)
            .map_err(internal)?;
        let cancel = CancellationToken::new();
        if background {
            runtime
                .background
                .lock()
                .await
                .insert(command_info.id.clone(), cancel.clone());
        } else {
            *foreground = Some(Foreground::Exec {
                command_id: command_info.id.clone(),
                cancel: cancel.clone(),
            });
        }
        drop(foreground);
        drop(connection);
        if !background {
            let mut info = runtime.info.write().await;
            info.status = SessionStatus::ExecRunning;
            info.current_command = Some(command.into());
            info.last_exit_code = None;
            info.updated_at = now;
            self.storage.upsert_session(&info).map_err(internal)?;
        }
        let manager = Arc::clone(self);
        let command_task = command_info.clone();
        let runtime_task = Arc::clone(&runtime);
        let timeout = timeout_seconds
            .unwrap_or(if background { 86400 } else { 300 })
            .clamp(1, 86400);
        tokio::spawn(async move {
            manager
                .run_exec(
                    runtime_task,
                    command_task,
                    channel,
                    cancel,
                    timeout,
                    background,
                )
                .await;
        });
        Ok(command_info)
    }

    async fn run_exec(
        self: Arc<Self>,
        runtime: Arc<SessionRuntime>,
        mut command: CommandInfo,
        mut channel: aissh_ssh::ExecChannel,
        cancel: CancellationToken,
        timeout_seconds: u64,
        background: bool,
    ) {
        let deadline = tokio::time::sleep(Duration::from_secs(timeout_seconds));
        tokio::pin!(deadline);
        let mut exit = None;
        loop {
            tokio::select! {
                _=cancel.cancelled()=>{let _=channel.cancel().await;command.status=CommandStatus::Cancelled;break;}
                _=&mut deadline=>{let _=channel.cancel().await;command.status=CommandStatus::TimedOut;break;}
                event=channel.next()=>match event {
                    Some(ChannelEvent::Data(data))=>self.record(&runtime,Some(&command.id),StreamKind::Stdout,data),
                    Some(ChannelEvent::ExtendedData(data))=>self.record(&runtime,Some(&command.id),StreamKind::Stderr,data),
                    Some(ChannelEvent::ExitStatus(code))=>exit=Some(code),
                    Some(ChannelEvent::Eof)=>{},
                    Some(ChannelEvent::Close)|None=>{command.status=command_status_from_exit(exit);break;}
                }
            }
        }
        command.exit_code = exit;
        command.finished_at = Some(Utc::now());
        command.last_sequence = runtime.sequence.load(Ordering::SeqCst);
        let _ = self.storage.update_command(&command);
        if let Ok(mut value) = runtime.last_activity.lock() {
            *value = Instant::now();
        }
        if background {
            runtime.background.lock().await.remove(&command.id);
        } else {
            let mut foreground = runtime.foreground.lock().await;
            if matches!(&*foreground,Some(Foreground::Exec{command_id,..}) if command_id==&command.id)
            {
                *foreground = None;
            }
            drop(foreground);
            let mut info = runtime.info.write().await;
            if info.status != SessionStatus::Closed {
                info.status = SessionStatus::Idle;
                info.current_command = None;
                info.last_exit_code = exit;
                info.updated_at = Utc::now();
                let _ = self.storage.upsert_session(&info);
            }
        }
    }

    pub async fn command_poll(
        &self,
        command_id: &str,
        after: u64,
        max_bytes: usize,
    ) -> ApiResult<(CommandInfo, Vec<TerminalEvent>, bool, Vec<String>)> {
        let command = self
            .storage
            .command(command_id)
            .map_err(internal)?
            .ok_or_else(|| {
                ErrorPayload::new(
                    "COMMAND_NOT_FOUND",
                    format!("command {command_id:?} does not exist"),
                )
            })?;
        let limit = max_bytes.clamp(1, 1024 * 1024);
        let (mut events, mut more) = self
            .storage
            .events_for_command(command_id, after, limit)
            .map_err(internal)?;
        if let Some(runtime) = self.sessions.read().await.get(&command.session_id) {
            append_live(
                &mut events,
                &mut more,
                &runtime.live_events,
                after,
                limit,
                Some(command_id),
            );
        }
        let mut warnings = Vec::new();
        if command.recording_truncated {
            warnings.push(
                "command output exceeded the configured recording limit; earlier output may no longer be available"
                    .into(),
            );
        }
        if after == 0 && events.is_empty() && command.status != CommandStatus::Running {
            warnings.push("command finished without producing stdout or stderr events".into());
        }
        Ok((command, events, more, warnings))
    }

    pub async fn command_cancel(&self, command_id: &str) -> ApiResult<()> {
        for runtime in self.sessions.read().await.values() {
            let foreground = runtime.foreground.lock().await;
            if let Some(Foreground::Exec {
                command_id: id,
                cancel,
            }) = &*foreground
            {
                if id == command_id {
                    cancel.cancel();
                    return Ok(());
                }
            }
            drop(foreground);
            if let Some(cancel) = runtime.background.lock().await.get(command_id) {
                cancel.cancel();
                return Ok(());
            }
        }
        Err(ErrorPayload::new(
            "COMMAND_NOT_RUNNING",
            format!("command {command_id:?} is not running"),
        ))
    }

    pub async fn shell_open(
        self: &Arc<Self>,
        session_id: &str,
        cols: u32,
        rows: u32,
        term: &str,
    ) -> ApiResult<SessionInfo> {
        let runtime = self.runtime(session_id).await?;
        let mut foreground = runtime.foreground.lock().await;
        if foreground.is_some() {
            return Err(ErrorPayload::new(
                "SESSION_BUSY",
                "session already has a foreground exec or PTY",
            )
            .retryable());
        }
        let mut connection = runtime.connection.lock().await;
        let (reader, writer) = connection
            .as_mut()
            .ok_or_else(|| {
                ErrorPayload::new("SESSION_DISCONNECTED", "SSH connection is not available")
            })?
            .open_pty(cols.clamp(20, 1000), rows.clamp(5, 500), term)
            .await
            .map_err(classify_error)?;
        let writer = Arc::new(writer);
        *foreground = Some(Foreground::Pty {
            writer: Arc::clone(&writer),
        });
        drop(foreground);
        drop(connection);
        {
            let mut info = runtime.info.write().await;
            info.status = SessionStatus::PtyOpen;
            info.current_command = Some("Interactive shell".into());
            info.updated_at = Utc::now();
            self.storage.upsert_session(&info).map_err(internal)?;
        }
        let manager = Arc::clone(self);
        let task_runtime = Arc::clone(&runtime);
        tokio::spawn(async move {
            manager.run_pty(task_runtime, reader).await;
        });
        Ok(runtime.info.read().await.clone())
    }

    async fn run_pty(
        self: Arc<Self>,
        runtime: Arc<SessionRuntime>,
        mut reader: aissh_ssh::PtyReader,
    ) {
        while let Some(data) = reader.next().await {
            if let Ok(mut value) = runtime.last_activity.lock() {
                *value = Instant::now();
            }
            self.record(&runtime, None, StreamKind::Pty, data);
        }
        runtime.foreground.lock().await.take();
        let mut info = runtime.info.write().await;
        if info.status != SessionStatus::Closed {
            info.status = SessionStatus::Idle;
            info.current_command = None;
            info.updated_at = Utc::now();
            let _ = self.storage.upsert_session(&info);
        }
    }

    pub async fn shell_write(&self, session_id: &str, data: &[u8]) -> ApiResult<()> {
        let runtime = self.runtime(session_id).await?;
        let foreground = runtime.foreground.lock().await;
        if let Some(Foreground::Pty { writer }) = &*foreground {
            writer.write(data).await.map_err(classify_error)?;
            if let Ok(mut value) = runtime.last_activity.lock() {
                *value = Instant::now();
            }
            Ok(())
        } else {
            Err(ErrorPayload::new("PTY_NOT_OPEN", "session has no open PTY"))
        }
    }
    pub async fn shell_resize(&self, session_id: &str, cols: u32, rows: u32) -> ApiResult<()> {
        let runtime = self.runtime(session_id).await?;
        let foreground = runtime.foreground.lock().await;
        if let Some(Foreground::Pty { writer }) = &*foreground {
            writer
                .resize(cols.clamp(20, 1000), rows.clamp(5, 500))
                .await
                .map_err(classify_error)
        } else {
            Err(ErrorPayload::new("PTY_NOT_OPEN", "session has no open PTY"))
        }
    }
    pub async fn shell_close(&self, session_id: &str) -> ApiResult<()> {
        let runtime = self.runtime(session_id).await?;
        let mut foreground = runtime.foreground.lock().await;
        if let Some(Foreground::Pty { writer }) = foreground.take() {
            writer.close().await.map_err(classify_error)
        } else {
            Err(ErrorPayload::new("PTY_NOT_OPEN", "session has no open PTY"))
        }
    }
    pub async fn shell_read(
        &self,
        session_id: &str,
        after: u64,
        max_bytes: usize,
    ) -> ApiResult<(Vec<CommandInfo>, Vec<TerminalEvent>, bool)> {
        let runtime = self.sessions.read().await.get(session_id).cloned();
        if runtime.is_none()
            && self
                .storage
                .session(session_id)
                .map_err(internal)?
                .is_none()
        {
            return Err(session_not_found(session_id));
        }
        let limit = max_bytes.clamp(1, 1024 * 1024);
        let (mut events, mut more) = self
            .storage
            .events_for_session(session_id, after, limit)
            .map_err(internal)?;
        if let Some(runtime) = runtime {
            append_live(
                &mut events,
                &mut more,
                &runtime.live_events,
                after,
                limit,
                None,
            );
        }
        let mut command_ids = events
            .iter()
            .filter_map(|event| event.command_id.clone())
            .collect::<std::collections::BTreeSet<_>>();
        if let Some(command) = self
            .storage
            .latest_command_for_session(session_id)
            .map_err(internal)?
        {
            command_ids.insert(command.id);
        }
        let commands = command_ids
            .into_iter()
            .filter_map(|id| self.storage.command(&id).transpose())
            .collect::<Result<Vec<_>, _>>()
            .map_err(internal)?;
        Ok((commands, events, more))
    }

    pub async fn reload_config(&self) -> ApiResult<()> {
        let config = Config::load(&self.paths)
            .map_err(|e| ErrorPayload::new("CONFIG_INVALID", e.to_string()))?;
        *self.config.write().await = Arc::new(config);
        Ok(())
    }

    pub async fn reap_idle(&self) {
        let config = self.config.read().await.clone();
        let timeout = Duration::from_secs(config.idle_timeout_seconds);
        let _ = self.storage.cleanup(config.retention_days);
        let sessions: Vec<_> = self.sessions.read().await.values().cloned().collect();
        for runtime in sessions {
            let elapsed = runtime
                .last_activity
                .lock()
                .map(|v| v.elapsed())
                .unwrap_or_default();
            if elapsed >= timeout
                && runtime.foreground.lock().await.is_none()
                && runtime.background.lock().await.is_empty()
            {
                let id = runtime.info.read().await.id.clone();
                let _ = self.close(&id).await;
            }
        }
    }

    fn record(
        &self,
        runtime: &SessionRuntime,
        command_id: Option<&str>,
        stream: StreamKind,
        payload: Vec<u8>,
    ) {
        let mut event = TerminalEvent {
            session_id: runtime.session_id.clone(),
            command_id: command_id.map(str::to_owned),
            sequence: runtime.sequence.fetch_add(1, Ordering::SeqCst) + 1,
            timestamp: Utc::now(),
            stream,
            payload,
            persisted: true,
        };
        if let Ok(mut value) = runtime.last_activity.lock() {
            *value = Instant::now();
        }
        let _ = self.storage.append_event(&mut event);
        if !event.persisted {
            runtime.recording_truncated.store(true, Ordering::Relaxed);
            if let Ok(mut live) = runtime.live_events.lock() {
                live.push_back(event);
                let mut bytes: usize = live.iter().map(|e| e.payload.len()).sum();
                while bytes > 4 * 1024 * 1024 {
                    if let Some(old) = live.pop_front() {
                        bytes = bytes.saturating_sub(old.payload.len())
                    } else {
                        break;
                    }
                }
            }
        }
    }
    async fn runtime(&self, id: &str) -> ApiResult<Arc<SessionRuntime>> {
        self.sessions
            .read()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| session_not_found(id))
    }
}

fn session_not_found(id: &str) -> ErrorPayload {
    ErrorPayload::new(
        "SESSION_NOT_FOUND",
        format!("session {id:?} does not exist"),
    )
}

fn internal(error: impl std::fmt::Display) -> ErrorPayload {
    ErrorPayload::new("INTERNAL", error.to_string())
}
fn classify_error(error: impl std::fmt::Display) -> ErrorPayload {
    let message = error.to_string();
    if let Some((code, detail)) = message.split_once(": ") {
        if code.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
            return ErrorPayload::new(code, detail);
        }
    }
    ErrorPayload::new("SSH_ERROR", message).retryable()
}

fn append_live(
    events: &mut Vec<TerminalEvent>,
    more: &mut bool,
    live: &StdMutex<VecDeque<TerminalEvent>>,
    after: u64,
    limit: usize,
    command_id: Option<&str>,
) {
    // Persisted events are always the earlier part of the sequence. Appending
    // live overflow while a database page remains would advance the cursor
    // past unseen persisted events.
    if *more {
        return;
    }
    let Ok(live) = live.lock() else { return };
    let mut size: usize = events.iter().map(|e| e.payload.len()).sum();
    for event in live.iter().filter(|e| {
        e.sequence > after && command_id.is_none_or(|id| e.command_id.as_deref() == Some(id))
    }) {
        if !events.is_empty() && size + event.payload.len() > limit {
            *more = true;
            break;
        }
        size += event.payload.len();
        events.push(event.clone());
    }
    events.sort_by_key(|e| e.sequence);
}

fn clearly_interactive(command: &str) -> bool {
    let words: Vec<_> = command.split_whitespace().collect();
    match words.first().copied() {
        Some("vi" | "vim" | "nano" | "less" | "more" | "watch") => true,
        Some("top") => !words.contains(&"-b"),
        _ => false,
    }
}

fn command_status_from_exit(exit: Option<u32>) -> CommandStatus {
    if exit == Some(0) {
        CommandStatus::Completed
    } else {
        CommandStatus::Failed
    }
}

pub fn events_response(
    command: Option<CommandInfo>,
    commands: Vec<CommandInfo>,
    events: Vec<TerminalEvent>,
    after: u64,
    more: bool,
    warnings: Vec<String>,
) -> ResponseData {
    let next_sequence = events.last().map(|e| e.sequence).unwrap_or(after);
    let delivery_complete = command
        .as_ref()
        .is_some_and(|value| value.status != CommandStatus::Running && !more);
    ResponseData::Events {
        command,
        commands,
        events,
        next_sequence,
        has_more: more,
        delivery_complete,
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn traces_historical_session_without_a_runtime() {
        let storage = Arc::new(Storage::open(std::path::Path::new(":memory:"), 1).unwrap());
        let now = Utc::now();
        storage
            .upsert_session(&SessionInfo {
                id: "historical-session".into(),
                target_id: "target".into(),
                target_name: "Target".into(),
                purpose: "history test".into(),
                client_name: "test client".into(),
                status: SessionStatus::Interrupted,
                current_command: None,
                last_exit_code: None,
                recording_truncated: false,
                host_fingerprint: None,
                created_at: now,
                updated_at: now,
                closed_at: Some(now),
            })
            .unwrap();
        storage
            .insert_command(&CommandInfo {
                id: "historical-command".into(),
                session_id: "historical-session".into(),
                command: "printf history".into(),
                status: CommandStatus::Completed,
                exit_code: Some(0),
                started_at: now,
                finished_at: Some(now),
                last_sequence: 1,
                recording_truncated: false,
            })
            .unwrap();
        let mut event = TerminalEvent {
            session_id: "historical-session".into(),
            command_id: Some("historical-command".into()),
            sequence: 1,
            timestamp: now,
            stream: StreamKind::Stdout,
            payload: b"history".to_vec(),
            persisted: true,
        };
        storage.append_event(&mut event).unwrap();

        let manager = SessionManager::new(
            Config::default_config(),
            Paths::under(std::path::PathBuf::from("/unused")),
            storage,
        );
        let status = manager.status("historical-session").await.unwrap();
        assert_eq!(status.status, SessionStatus::Interrupted);

        let (commands, events, has_more) = manager
            .shell_read("historical-session", 0, 64 * 1024)
            .await
            .unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].payload, b"history");
        assert!(!has_more);

        let error = manager.status("missing-session").await.unwrap_err();
        assert_eq!(error.code, "SESSION_NOT_FOUND");
    }

    #[test]
    fn stable_error_code_is_preserved() {
        let error = classify_error("AUTH_FAILED: rejected");
        assert_eq!(error.code, "AUTH_FAILED");
    }
    #[test]
    fn recognizes_explicit_full_screen_commands() {
        assert!(clearly_interactive("vim /etc/hosts"));
        assert!(!clearly_interactive("top -b -n 1"));
    }
    #[test]
    fn maps_exit_status_only_after_channel_completion() {
        assert_eq!(command_status_from_exit(Some(0)), CommandStatus::Completed);
        assert_eq!(command_status_from_exit(Some(2)), CommandStatus::Failed);
        assert_eq!(command_status_from_exit(None), CommandStatus::Failed);
    }

    #[test]
    fn keeps_empty_page_cursor_and_marks_terminal_delivery_complete() {
        let command = CommandInfo {
            id: "c".into(),
            session_id: "s".into(),
            command: "true".into(),
            status: CommandStatus::Completed,
            exit_code: Some(0),
            started_at: Utc::now(),
            finished_at: Some(Utc::now()),
            last_sequence: 41,
            recording_truncated: false,
        };
        let response = events_response(
            Some(command.clone()),
            vec![command],
            vec![],
            41,
            false,
            vec![],
        );
        assert!(matches!(
            response,
            ResponseData::Events {
                next_sequence: 41,
                delivery_complete: true,
                ..
            }
        ));
    }

    #[test]
    fn does_not_mix_live_tail_into_an_incomplete_persisted_page() {
        let mut events = vec![];
        let mut more = true;
        let live = StdMutex::new(VecDeque::from([TerminalEvent {
            session_id: "s".into(),
            command_id: Some("c".into()),
            sequence: 3000,
            timestamp: Utc::now(),
            stream: StreamKind::Stdout,
            payload: b"tail".to_vec(),
            persisted: false,
        }]));
        append_live(&mut events, &mut more, &live, 0, 1024, Some("c"));
        assert!(events.is_empty());
    }
}
