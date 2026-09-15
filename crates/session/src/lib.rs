use aissh_config::{Config, Paths};
use aissh_protocol::{
    CommandInfo, CommandStatus, CommandSummary, ErrorPayload, EventProgress, FileContentInfo,
    FileKind, FileOutcome, FileStatInfo, MAX_WAIT_SECONDS, ResponseData, SessionInfo,
    SessionStatus, StreamKind, TargetSummary, TerminalEvent,
};
use aissh_ssh::{
    ChannelEvent, PtyWriter, RemoteHash, SftpSession, SshConnection, build_command, shell_quote,
};
use aissh_storage::Storage;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    path::PathBuf,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, RwLock, watch};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

type ApiResult<T> = Result<T, ErrorPayload>;

/// Ceiling for bytes returned by a single file read.
const MAX_READ_BYTES: usize = 1024 * 1024;
/// Ceiling for the stdout of an internal helper command (hashing, sizing).
const HELPER_OUTPUT_BYTES: usize = 64 * 1024;
/// Bound on a privileged (sudo) write, which streams its whole payload.
const SUDO_WRITE_TIMEOUT: Duration = Duration::from_secs(300);
/// Ceiling for rewriting a file in place to honor an append. Above this the
/// caller is told to upload a full replacement instead of silently degrading
/// the verification guarantee.
const APPEND_READ_LIMIT: usize = 8 * 1024 * 1024;

pub struct SessionManager {
    config: RwLock<Arc<Config>>,
    paths: Paths,
    storage: Arc<Storage>,
    sessions: RwLock<HashMap<String, Arc<SessionRuntime>>>,
}

struct SessionRuntime {
    session_id: String,
    info: RwLock<SessionInfo>,
    connection: Mutex<Option<Arc<SshConnection>>>,
    foreground: Mutex<Option<Foreground>>,
    background: Mutex<HashMap<String, CancellationToken>>,
    sequence: AtomicU64,
    recording_truncated: AtomicBool,
    live_tail_dropped: AtomicBool,
    last_activity: StdMutex<Instant>,
    last_output_at: StdMutex<Option<Instant>>,
    live_events: StdMutex<VecDeque<TerminalEvent>>,
    /// Bumped after every recorded event and on every command transition, so a
    /// polling caller can block instead of sleeping in a client-side loop.
    events_tx: watch::Sender<u64>,
}

enum Foreground {
    Exec {
        command_id: String,
        cancel: CancellationToken,
    },
    Pty {
        writer: Arc<PtyWriter>,
        /// Identifies this PTY so an intentional close is not mistaken for a
        /// dropped transport when the reader ends.
        id: String,
    },
}

/// One page of command output plus how much of the stream it represents.
#[derive(Debug)]
pub struct PollPage {
    pub command: CommandInfo,
    pub events: Vec<TerminalEvent>,
    pub more: bool,
    pub warnings: Vec<String>,
    pub progress: EventProgress,
}

/// One page of session-scoped PTY output.
#[derive(Debug)]
pub struct ShellPage {
    pub commands: Vec<CommandInfo>,
    pub events: Vec<TerminalEvent>,
    pub more: bool,
    pub progress: EventProgress,
}

/// A freshly read command page, before progress and warnings are attached.
struct CommandPage {
    command: CommandInfo,
    events: Vec<TerminalEvent>,
    more: bool,
    runtime: Option<Arc<SessionRuntime>>,
}

impl CommandPage {
    /// Nothing to return yet and the command may still produce output.
    fn quiet(&self) -> bool {
        self.events.is_empty() && !self.more && self.command.status == CommandStatus::Running
    }
}

fn progress_for(
    command: &CommandInfo,
    runtime: Option<&Arc<SessionRuntime>>,
    timed_out: bool,
    waited_seconds: u64,
) -> EventProgress {
    let running = command.status == CommandStatus::Running;
    EventProgress {
        // A finished command has no meaningful "time since last output".
        seconds_since_last_output: if running {
            seconds_since_output(runtime)
        } else {
            None
        },
        live_tail_dropped: runtime
            .is_some_and(|runtime| runtime.live_tail_dropped.load(Ordering::Relaxed)),
        timed_out,
        waited_seconds,
    }
}

/// A session has no single "running" command, so a PTY read always reports the
/// session's most recent output time.
fn shell_progress(
    timed_out: bool,
    started: Instant,
    runtime: Option<&Arc<SessionRuntime>>,
) -> EventProgress {
    EventProgress {
        seconds_since_last_output: seconds_since_output(runtime),
        live_tail_dropped: runtime
            .is_some_and(|runtime| runtime.live_tail_dropped.load(Ordering::Relaxed)),
        timed_out,
        waited_seconds: started.elapsed().as_secs(),
    }
}

fn seconds_since_output(runtime: Option<&Arc<SessionRuntime>>) -> Option<u64> {
    runtime
        .and_then(|runtime| runtime.last_output_at.lock().ok().and_then(|value| *value))
        .map(|instant| instant.elapsed().as_secs())
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
            live_tail_dropped: AtomicBool::new(false),
            last_activity: StdMutex::new(Instant::now()),
            last_output_at: StdMutex::new(None),
            live_events: StdMutex::new(VecDeque::new()),
            events_tx: watch::channel(0).0,
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
                *runtime.connection.lock().await = Some(Arc::new(connection));
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

    /// Returns a usable connection, transparently reconnecting when the
    /// transport has gone away.
    ///
    /// A dropped connection is never retried against the same handle: the dead
    /// handle is discarded first, because `russh` keeps the closed handle in
    /// place and would otherwise turn the next command into a confusing generic
    /// `SSH_ERROR` instead of a clean reconnect.
    async fn connection_for(
        self: &Arc<Self>,
        runtime: &Arc<SessionRuntime>,
    ) -> ApiResult<Arc<SshConnection>> {
        {
            let slot = runtime.connection.lock().await;
            if let Some(connection) = slot.as_ref()
                && !connection.is_closed()
            {
                return Ok(Arc::clone(connection));
            }
        }
        let connection = self.connect_runtime(runtime).await?;
        let mut slot = runtime.connection.lock().await;
        // Another caller may have reconnected while we were dialing.
        if let Some(existing) = slot.as_ref()
            && !existing.is_closed()
        {
            return Ok(Arc::clone(existing));
        }
        *slot = Some(Arc::clone(&connection));
        Ok(connection)
    }

    async fn connect_runtime(
        &self,
        runtime: &Arc<SessionRuntime>,
    ) -> ApiResult<Arc<SshConnection>> {
        let (target_id, status) = {
            let info = runtime.info.read().await;
            (info.target_id.clone(), info.status.clone())
        };
        if status == SessionStatus::Closed {
            return Err(ErrorPayload::new("SESSION_CLOSED", "session is closed"));
        }
        let config = self.config.read().await.clone();
        let target = config
            .targets
            .iter()
            .find(|target| target.id == target_id)
            .cloned()
            .ok_or_else(|| {
                ErrorPayload::new(
                    "TARGET_NOT_FOUND",
                    format!("target {target_id:?} no longer exists in the configuration"),
                )
            })?;
        let attempts = config.reconnect_attempts.clamp(1, 10);
        let backoff = Duration::from_secs(config.reconnect_backoff_seconds.min(60));
        let mut last = None;
        for attempt in 1..=attempts {
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
                    let mut info = runtime.info.write().await;
                    if info.status != SessionStatus::Closed {
                        // Only a session that had no live work is "Ready"; leave
                        // an in-flight exec or PTY status alone.
                        if matches!(
                            info.status,
                            SessionStatus::Connecting
                                | SessionStatus::Disconnected
                                | SessionStatus::Failed
                        ) {
                            info.status = SessionStatus::Ready;
                        }
                        info.host_fingerprint = Some(fingerprint);
                        info.updated_at = Utc::now();
                        let _ = self.storage.upsert_session(&info);
                    }
                    return Ok(Arc::new(connection));
                }
                Err(error) => {
                    last = Some(classify_error(error));
                    if attempt < attempts && !backoff.is_zero() {
                        tokio::time::sleep(backoff).await;
                    }
                }
            }
        }
        let error = last.unwrap_or_else(|| ErrorPayload::new("SSH_ERROR", "reconnect failed"));
        let mut info = runtime.info.write().await;
        if info.status != SessionStatus::Closed {
            info.status = SessionStatus::Disconnected;
            info.current_command = None;
            info.updated_at = Utc::now();
            let _ = self.storage.upsert_session(&info);
        }
        Err(error)
    }

    /// Drops a handle that failed mid-operation so the next call reconnects.
    async fn discard_connection(&self, runtime: &Arc<SessionRuntime>) {
        runtime.connection.lock().await.take();
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
                Foreground::Pty { writer, .. } => {
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
        let connection = self.connection_for(&runtime).await?;
        let channel = match connection.open_exec(&remote_command).await {
            Ok(channel) => channel,
            Err(error) => {
                self.discard_connection(&runtime).await;
                return Err(classify_error(error));
            }
        };
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
            output_bytes: 0,
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
                    Some(ChannelEvent::Close)|None=>{
                        // No exit status means the transport ended the channel, not
                        // the remote shell. Reporting that as `Failed` would make a
                        // dropped connection indistinguishable from a non-zero exit.
                        command.status=channel_end_status(exit);
                        break;
                    }
                }
            }
        }
        let interrupted = command.status == CommandStatus::Interrupted;
        command.exit_code = exit;
        command.finished_at = Some(Utc::now());
        command.last_sequence = runtime.sequence.load(Ordering::SeqCst);
        command.output_bytes = self
            .storage
            .command(&command.id)
            .ok()
            .flatten()
            .map(|value| value.output_bytes)
            .unwrap_or_default();
        let _ = self.storage.update_command(&command);
        if let Ok(mut value) = runtime.last_activity.lock() {
            *value = Instant::now();
        }
        if interrupted {
            // The handle that just dropped cannot serve anything else.
            self.discard_connection(&runtime).await;
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
                info.status = if interrupted {
                    SessionStatus::Disconnected
                } else {
                    SessionStatus::Idle
                };
                info.current_command = None;
                info.last_exit_code = exit;
                info.updated_at = Utc::now();
                let _ = self.storage.upsert_session(&info);
            }
        }
        // Wake any waiter that is blocked on this command's stream.
        runtime
            .events_tx
            .send_modify(|value| *value = value.wrapping_add(1));
    }

    pub async fn command_poll(
        &self,
        command_id: &str,
        after: u64,
        max_bytes: usize,
        wait_seconds: u64,
    ) -> ApiResult<PollPage> {
        let started = Instant::now();
        let budget = Duration::from_secs(wait_seconds.min(MAX_WAIT_SECONDS));
        let deadline = started + budget;
        let limit = max_bytes.clamp(1, MAX_READ_BYTES);
        // Subscribe before the first page read: a write that lands during the
        // read is then either already in the page or has moved the version we
        // wait on, so no wakeup can be lost.
        let mut receiver = self.events_receiver(command_id).await;
        loop {
            let page = self.read_command_page(command_id, after, limit).await?;
            let quiet = page.quiet();
            // Return when output is waiting, when the command is terminal and
            // fully drained, or when the caller did not ask to block.
            if !quiet || budget.is_zero() {
                return Ok(self.finish_poll(page, after, false, started));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Some(receiver) = receiver.as_mut() else {
                return Ok(self.finish_poll(page, after, true, started));
            };
            let woke_on_output = remaining > Duration::ZERO
                && matches!(
                    tokio::time::timeout(remaining, receiver.changed()).await,
                    Ok(Ok(()))
                );
            if woke_on_output {
                continue;
            }
            // Budget expired, or the session's sender was dropped.
            let page = self.read_command_page(command_id, after, limit).await?;
            return Ok(self.finish_poll(page, after, true, started));
        }
    }

    async fn read_command_page(
        &self,
        command_id: &str,
        after: u64,
        limit: usize,
    ) -> ApiResult<CommandPage> {
        let command = self.load_command(command_id)?;
        let (mut events, mut more) = self
            .storage
            .events_for_command(command_id, after, limit)
            .map_err(internal)?;
        let runtime = self.sessions.read().await.get(&command.session_id).cloned();
        if let Some(runtime) = &runtime {
            append_live(
                &mut events,
                &mut more,
                &runtime.live_events,
                after,
                limit,
                Some(command_id),
            );
        }
        Ok(CommandPage {
            command,
            events,
            more,
            runtime,
        })
    }

    fn load_command(&self, command_id: &str) -> ApiResult<CommandInfo> {
        self.storage
            .command(command_id)
            .map_err(internal)?
            .ok_or_else(|| {
                ErrorPayload::new(
                    "COMMAND_NOT_FOUND",
                    format!("command {command_id:?} does not exist"),
                )
            })
    }

    /// A receiver that fires whenever this session records output.
    async fn events_receiver(&self, command_id: &str) -> Option<watch::Receiver<u64>> {
        let session_id = self.storage.command(command_id).ok().flatten()?.session_id;
        let runtime = self.sessions.read().await.get(&session_id).cloned()?;
        Some(runtime.events_tx.subscribe())
    }

    fn finish_poll(
        &self,
        page: CommandPage,
        after: u64,
        timed_out: bool,
        started: Instant,
    ) -> PollPage {
        let CommandPage {
            command,
            events,
            more,
            runtime,
        } = page;
        let mut warnings = Vec::new();
        if command.recording_truncated {
            warnings.push(
                "the session recording limit dropped earlier output; it is no longer available from storage"
                    .into(),
            );
        }
        if runtime
            .as_ref()
            .is_some_and(|value| value.live_tail_dropped.load(Ordering::Relaxed))
        {
            warnings.push(
                "the in-memory overflow buffer dropped its oldest output; some output is unrecoverable"
                    .into(),
            );
        }
        if command.status == CommandStatus::Interrupted {
            if command.output_bytes > 0 {
                warnings.push(
                    "the SSH transport dropped before an exit status arrived; this command did produce output, so the remote process started and may still be running"
                        .into(),
                );
            } else {
                warnings.push(
                    "the SSH transport dropped before an exit status arrived and no output was received; whether the remote process started is unknown"
                        .into(),
                );
            }
        }
        if after == 0 && events.is_empty() && command.status != CommandStatus::Running {
            warnings.push("command finished without producing stdout or stderr events".into());
        }
        PollPage {
            progress: progress_for(
                &command,
                runtime.as_ref(),
                timed_out,
                started.elapsed().as_secs(),
            ),
            command,
            events,
            more,
            warnings,
        }
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
        let connection = self.connection_for(&runtime).await?;
        let (reader, writer) = match connection
            .open_pty(cols.clamp(20, 1000), rows.clamp(5, 500), term)
            .await
        {
            Ok(value) => value,
            Err(error) => {
                self.discard_connection(&runtime).await;
                return Err(classify_error(error));
            }
        };
        let writer = Arc::new(writer);
        let pty_id = Uuid::new_v4().to_string();
        *foreground = Some(Foreground::Pty {
            writer: Arc::clone(&writer),
            id: pty_id.clone(),
        });
        drop(foreground);
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
            manager.run_pty(task_runtime, reader, pty_id).await;
        });
        Ok(runtime.info.read().await.clone())
    }

    async fn run_pty(
        self: Arc<Self>,
        runtime: Arc<SessionRuntime>,
        mut reader: aissh_ssh::PtyReader,
        pty_id: String,
    ) {
        while let Some(data) = reader.next().await {
            if let Ok(mut value) = runtime.last_activity.lock() {
                *value = Instant::now();
            }
            self.record(&runtime, None, StreamKind::Pty, data);
        }
        // Only claim the PTY is gone if it is still ours: an explicit
        // `ssh_shell_close` already took the slot and means the end was intended.
        let intentional = {
            let mut foreground = runtime.foreground.lock().await;
            match &*foreground {
                Some(Foreground::Pty { id, .. }) if id == &pty_id => {
                    *foreground = None;
                    false
                }
                _ => true,
            }
        };
        if !intentional {
            self.discard_connection(&runtime).await;
        }
        let mut info = runtime.info.write().await;
        if info.status != SessionStatus::Closed {
            info.status = if intentional {
                SessionStatus::Idle
            } else {
                SessionStatus::Disconnected
            };
            info.current_command = None;
            info.updated_at = Utc::now();
            let _ = self.storage.upsert_session(&info);
        }
        runtime
            .events_tx
            .send_modify(|value| *value = value.wrapping_add(1));
    }

    pub async fn shell_write(&self, session_id: &str, data: &[u8]) -> ApiResult<()> {
        let runtime = self.runtime(session_id).await?;
        let foreground = runtime.foreground.lock().await;
        if let Some(Foreground::Pty { writer, .. }) = &*foreground {
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
        if let Some(Foreground::Pty { writer, .. }) = &*foreground {
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
        if let Some(Foreground::Pty { writer, .. }) = foreground.take() {
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
        wait_seconds: u64,
    ) -> ApiResult<ShellPage> {
        let started = Instant::now();
        let budget = Duration::from_secs(wait_seconds.min(MAX_WAIT_SECONDS));
        let deadline = started + budget;
        let limit = max_bytes.clamp(1, MAX_READ_BYTES);
        let mut receiver = {
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
            runtime.map(|runtime| runtime.events_tx.subscribe())
        };
        loop {
            let (commands, events, more, runtime) =
                self.read_shell_page(session_id, after, limit).await?;
            let quiet = events.is_empty() && !more;
            if !quiet || budget.is_zero() {
                return Ok(ShellPage {
                    progress: shell_progress(false, started, runtime.as_ref()),
                    commands,
                    events,
                    more,
                });
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Some(receiver) = receiver.as_mut() else {
                return Ok(ShellPage {
                    progress: shell_progress(true, started, runtime.as_ref()),
                    commands,
                    events,
                    more,
                });
            };
            let woke = remaining > Duration::ZERO
                && matches!(
                    tokio::time::timeout(remaining, receiver.changed()).await,
                    Ok(Ok(()))
                );
            if woke {
                continue;
            }
            let (commands, events, more, runtime) =
                self.read_shell_page(session_id, after, limit).await?;
            return Ok(ShellPage {
                progress: shell_progress(true, started, runtime.as_ref()),
                commands,
                events,
                more,
            });
        }
    }

    async fn read_shell_page(
        &self,
        session_id: &str,
        after: u64,
        limit: usize,
    ) -> ApiResult<(
        Vec<CommandInfo>,
        Vec<TerminalEvent>,
        bool,
        Option<Arc<SessionRuntime>>,
    )> {
        let runtime = self.sessions.read().await.get(session_id).cloned();
        let (mut events, mut more) = self
            .storage
            .events_for_session(session_id, after, limit)
            .map_err(internal)?;
        if let Some(runtime) = &runtime {
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
        Ok((commands, events, more, runtime))
    }

    pub async fn reload_config(&self) -> ApiResult<()> {
        let config = Config::load(&self.paths)
            .map_err(|e| ErrorPayload::new("CONFIG_INVALID", e.to_string()))?;
        *self.config.write().await = Arc::new(config);
        Ok(())
    }

    /// Commands across every session, newest first.
    pub async fn commands(
        &self,
        session_id: Option<&str>,
        include_finished: bool,
        limit: u32,
    ) -> ApiResult<Vec<CommandSummary>> {
        let mut summaries = self
            .storage
            .commands(session_id, include_finished, limit.clamp(1, 500) as usize)
            .map_err(internal)?;
        // Decorate the live ones with data only the runtime knows.
        let sessions = self.sessions.read().await;
        for summary in &mut summaries {
            if summary.status != CommandStatus::Running {
                continue;
            }
            let Some(runtime) = sessions.get(&summary.session_id) else {
                continue;
            };
            summary.seconds_since_last_output = seconds_since_output(Some(runtime));
        }
        Ok(summaries)
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
                        bytes = bytes.saturating_sub(old.payload.len());
                        // This loss used to be completely silent.
                        runtime.live_tail_dropped.store(true, Ordering::Relaxed);
                    } else {
                        break;
                    }
                }
            }
        }
        if let Ok(mut value) = runtime.last_output_at.lock() {
            *value = Some(Instant::now());
        }
        // Announced only after the event is readable, so a blocked poller never
        // wakes to an empty page.
        runtime
            .events_tx
            .send_modify(|value| *value = value.wrapping_add(1));
    }
    async fn runtime(&self, id: &str) -> ApiResult<Arc<SessionRuntime>> {
        self.sessions
            .read()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| session_not_found(id))
    }

    /// A live connection plus an SFTP subsystem channel on it.
    ///
    /// The connection `Arc` is returned alongside so the caller can reach an
    /// exec channel for remote hashing, which SFTP cannot provide.
    async fn sftp_channel(
        self: &Arc<Self>,
        session_id: &str,
    ) -> ApiResult<(SftpSession, Arc<SshConnection>, Arc<SessionRuntime>)> {
        let runtime = self.runtime(session_id).await?;
        let connection = self.connection_for(&runtime).await?;
        match connection.open_sftp().await {
            Ok(sftp) => Ok((sftp, connection, runtime)),
            Err(error) => {
                self.discard_connection(&runtime).await;
                Err(classify_error(error))
            }
        }
    }

    pub async fn file_stat(
        self: &Arc<Self>,
        session_id: &str,
        remote_path: &str,
        hash: bool,
    ) -> ApiResult<FileStatInfo> {
        let (sftp, connection, _) = self.sftp_channel(session_id).await?;
        let metadata = aissh_ssh::stat(&sftp, remote_path)
            .await
            .map_err(classify_error)?;
        // SFTP's STAT follows symlinks, so the reported kind describes what the
        // path resolves to rather than the link itself.
        let kind = if metadata.is_dir() {
            FileKind::Directory
        } else if metadata.is_regular() {
            FileKind::File
        } else {
            FileKind::Other
        };
        let sha256 = if hash && kind == FileKind::File {
            remote_sha256(&connection, remote_path, false).await
        } else {
            None
        };
        Ok(FileStatInfo {
            path: remote_path.to_owned(),
            kind,
            size: metadata.len(),
            mode: metadata.permissions,
            uid: metadata.uid,
            gid: metadata.gid,
            user: metadata.user.clone(),
            group: metadata.group.clone(),
            modified_at: metadata.modified().ok().map(DateTime::<Utc>::from),
            sha256,
        })
    }

    pub async fn file_read(
        self: &Arc<Self>,
        session_id: &str,
        remote_path: &str,
        max_bytes: usize,
        sudo: bool,
    ) -> ApiResult<FileContentInfo> {
        let limit = max_bytes.clamp(1, MAX_READ_BYTES);
        let runtime = self.runtime(session_id).await?;
        let connection = self.connection_for(&runtime).await?;
        let sha256 = if sudo {
            remote_sha256(&connection, remote_path, true).await
        } else {
            None
        };
        let (data, size, truncated) = if sudo {
            // SFTP runs as the login user and cannot open a root-owned path, so
            // the privileged read goes through exec instead.
            let known = sudo_file_size(&connection, remote_path).await?;
            let data = sudo_read_chunk(&connection, remote_path, limit).await?;
            let returned = data.len() as u64;
            let truncated = known.is_some_and(|size| size > returned);
            (data, known.unwrap_or(returned), truncated)
        } else {
            let (sftp, _, _) = self.sftp_channel(session_id).await?;
            aissh_ssh::read_capped(&sftp, remote_path, limit)
                .await
                .map_err(classify_error)?
        };
        let is_utf8 = std::str::from_utf8(&data).is_ok();
        Ok(FileContentInfo {
            path: remote_path.to_owned(),
            size,
            bytes_returned: data.len(),
            truncated,
            is_utf8,
            sha256: sha256.filter(|_| !truncated),
            data,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn file_write(
        self: &Arc<Self>,
        session_id: &str,
        remote_path: &str,
        data: &[u8],
        append: bool,
        create_dirs: bool,
        mode: Option<u32>,
        sudo: bool,
        if_changed: bool,
    ) -> ApiResult<FileOutcome> {
        let runtime = self.runtime(session_id).await?;
        let connection = self.connection_for(&runtime).await?;
        let digest = aissh_ssh::hex_digest(data);
        if if_changed && !append {
            // "Write only what changed": skip identical content entirely.
            if let Some(existing) = remote_sha256(&connection, remote_path, sudo).await
                && existing == digest
            {
                return Ok(FileOutcome {
                    path: remote_path.to_owned(),
                    bytes: data.len() as u64,
                    sha256: Some(digest),
                    verified: true,
                    changed: false,
                    mode,
                    local_path: None,
                });
            }
        }
        if sudo {
            if create_dirs {
                let parent = aissh_ssh::transfer::parent_of(remote_path);
                if let Some(parent) = parent {
                    sudo_make_dir(&connection, &parent).await?;
                }
            }
            sudo_write(&connection, remote_path, data, append).await?;
            if let Some(mode) = mode
                && let Err(error) = sudo_chmod(&connection, remote_path, mode).await
            {
                // The bytes are already in place; report the transfer, not the
                // cosmetic failure.
                tracing::warn!(%error, path = remote_path, "cannot apply mode after a privileged write");
            }
            let verified = remote_sha256(&connection, remote_path, true)
                .await
                .is_some_and(|actual| actual == digest);
            if !verified {
                return Err(ErrorPayload::new(
                    "TRANSFER_VERIFY_FAILED",
                    format!("{remote_path} does not match the bytes that were sent"),
                ));
            }
            return Ok(FileOutcome {
                path: remote_path.to_owned(),
                bytes: data.len() as u64,
                sha256: Some(digest),
                verified: true,
                changed: true,
                mode,
                local_path: None,
            });
        }
        let (sftp, _, _) = self.sftp_channel(session_id).await?;
        if create_dirs && let Some(parent) = aissh_ssh::transfer::parent_of(remote_path) {
            aissh_ssh::make_dir(&sftp, &parent, true, None)
                .await
                .map_err(classify_error)?;
        }
        if append {
            // Append is folded into a verified rewrite rather than an SFTP
            // APPEND, so the result is still digest-checked and atomic. That
            // needs the existing content, which bounds how large a file can be
            // appended to in place.
            let (existing, size, _) = aissh_ssh::read_capped(&sftp, remote_path, APPEND_READ_LIMIT)
                .await
                .map_err(classify_error)?;
            if size > existing.len() as u64 {
                return Err(ErrorPayload::new(
                    "INVALID_ARGUMENT",
                    format!(
                        "{remote_path} is {size} bytes, above the {} byte in-place append limit; upload the full replacement instead",
                        APPEND_READ_LIMIT
                    ),
                ));
            }
            let mut combined = existing;
            combined.extend_from_slice(data);
            let hasher = ExecRemoteHash {
                connection: Arc::clone(&connection),
            };
            let outcome =
                aissh_ssh::write_staged(&sftp, remote_path, &combined, mode, Some(&hasher))
                    .await
                    .map_err(classify_error)?;
            return Ok(to_file_outcome(remote_path, outcome, mode, None));
        }
        let hasher = ExecRemoteHash {
            connection: Arc::clone(&connection),
        };
        let outcome = aissh_ssh::write_staged(&sftp, remote_path, data, mode, Some(&hasher))
            .await
            .map_err(classify_error)?;
        Ok(to_file_outcome(remote_path, outcome, mode, None))
    }

    pub async fn file_upload(
        self: &Arc<Self>,
        session_id: &str,
        local_path: &str,
        remote_path: &str,
        create_dirs: bool,
        mode: Option<u32>,
        verify: bool,
    ) -> ApiResult<FileOutcome> {
        let local = local_path_for(local_path)?;
        let (sftp, connection, _) = self.sftp_channel(session_id).await?;
        if create_dirs && let Some(parent) = aissh_ssh::transfer::parent_of(remote_path) {
            aissh_ssh::make_dir(&sftp, &parent, true, None)
                .await
                .map_err(classify_error)?;
        }
        let hasher = ExecRemoteHash {
            connection: Arc::clone(&connection),
        };
        let outcome = aissh_ssh::upload_staged(
            &sftp,
            &local,
            remote_path,
            mode,
            verify.then_some(&hasher as &dyn RemoteHash),
        )
        .await
        .map_err(classify_error)?;
        Ok(to_file_outcome(remote_path, outcome, mode, Some(local)))
    }

    pub async fn file_download(
        self: &Arc<Self>,
        session_id: &str,
        remote_path: &str,
        local_path: &str,
        overwrite: bool,
        create_dirs: bool,
        verify: bool,
    ) -> ApiResult<FileOutcome> {
        let local = local_path_for(local_path)?;
        if local.exists() && !overwrite {
            return Err(ErrorPayload::new(
                "FILE_EXISTS",
                format!(
                    "{} already exists; pass overwrite=true to replace it",
                    local.display()
                ),
            ));
        }
        if create_dirs
            && let Some(parent) = local.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| classify_error(aissh_ssh::transfer::local_error(error, parent)))?;
        }
        let (sftp, connection, _) = self.sftp_channel(session_id).await?;
        let hasher = ExecRemoteHash {
            connection: Arc::clone(&connection),
        };
        let outcome = aissh_ssh::download_staged(
            &sftp,
            remote_path,
            &local,
            verify.then_some(&hasher as &dyn RemoteHash),
        )
        .await
        .map_err(classify_error)?;
        Ok(to_file_outcome(remote_path, outcome, None, Some(local)))
    }

    pub async fn file_mkdir(
        self: &Arc<Self>,
        session_id: &str,
        remote_path: &str,
        parents: bool,
        mode: Option<u32>,
    ) -> ApiResult<()> {
        let (sftp, _, _) = self.sftp_channel(session_id).await?;
        aissh_ssh::make_dir(&sftp, remote_path, parents, mode)
            .await
            .map_err(classify_error)
    }
}

fn session_not_found(id: &str) -> ErrorPayload {
    ErrorPayload::new(
        "SESSION_NOT_FOUND",
        format!("session {id:?} does not exist"),
    )
}

/// Resolves a client-supplied local path.
///
/// Absolute (or `~`-anchored) paths only: the daemon's own working directory is
/// meaningless to a caller, so a relative path would resolve somewhere
/// surprising.
fn local_path_for(value: &str) -> ApiResult<PathBuf> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ErrorPayload::new(
            "INVALID_ARGUMENT",
            "local_path must not be empty",
        ));
    }
    let path = if trimmed == "~" || trimmed.starts_with("~/") {
        let home = std::env::var_os("HOME").ok_or_else(|| {
            ErrorPayload::new("LOCAL_IO_ERROR", "cannot expand ~ because HOME is unset")
        })?;
        let rest = trimmed.trim_start_matches('~').trim_start_matches('/');
        PathBuf::from(home).join(rest)
    } else {
        PathBuf::from(trimmed)
    };
    if !path.is_absolute() {
        return Err(ErrorPayload::new(
            "INVALID_ARGUMENT",
            format!(
                "local_path must be absolute or start with ~: {}",
                path.display()
            ),
        ));
    }
    Ok(path)
}

fn to_file_outcome(
    path: &str,
    outcome: aissh_ssh::TransferOutcome,
    mode: Option<u32>,
    local: Option<PathBuf>,
) -> FileOutcome {
    FileOutcome {
        path: path.to_owned(),
        bytes: outcome.bytes,
        sha256: Some(outcome.sha256),
        verified: outcome.verified,
        changed: outcome.changed,
        mode,
        local_path: local.map(|value| value.to_string_lossy().into_owned()),
    }
}

/// Hashes a remote file by asking the remote host to do it, since SFTP has no
/// hashing operation of its own.
struct ExecRemoteHash {
    connection: Arc<SshConnection>,
}

#[async_trait]
impl RemoteHash for ExecRemoteHash {
    async fn sha256(&self, remote_path: &str) -> Option<String> {
        remote_sha256(&self.connection, remote_path, false).await
    }
}

/// Remote SHA-256, or `None` when the host has no usable hashing tool.
///
/// A missing tool downgrades the transfer to "unverified" instead of failing it,
/// so a minimal server image can still receive files.
async fn remote_sha256(connection: &SshConnection, path: &str, sudo: bool) -> Option<String> {
    let prefix = if sudo { "sudo -n " } else { "" };
    let quoted = shell_quote(path);
    let command = format!(
        "if command -v sha256sum >/dev/null 2>&1; then {prefix}sha256sum -- {quoted}; \
         elif command -v shasum >/dev/null 2>&1; then {prefix}shasum -a 256 -- {quoted}; \
         else exit 3; fi"
    );
    let output = exec_helper(connection, &command, HELPER_OUTPUT_BYTES)
        .await
        .ok()?;
    String::from_utf8(output)
        .ok()?
        .split_whitespace()
        .next()
        .filter(|digest| digest.len() == 64)
        .map(|digest| digest.to_ascii_lowercase())
}

/// Runs a helper command and returns its stdout, failing on a non-zero exit.
async fn exec_helper(
    connection: &SshConnection,
    command: &str,
    limit: usize,
) -> ApiResult<Vec<u8>> {
    let mut channel = connection
        .open_exec(command)
        .await
        .map_err(classify_error)?;
    let (stdout, stderr, exit, _) = channel.collect(limit).await.map_err(classify_error)?;
    if exit != Some(0) {
        let detail = String::from_utf8_lossy(&stderr).trim().to_owned();
        return Err(ErrorPayload::new(
            "REMOTE_COMMAND_FAILED",
            if detail.is_empty() {
                format!("helper command exited with {exit:?}")
            } else {
                detail
            },
        ));
    }
    Ok(stdout)
}

async fn sudo_file_size(connection: &SshConnection, path: &str) -> ApiResult<Option<u64>> {
    let command = format!("sudo -n wc -c -- {}", shell_quote(path));
    let output = exec_helper(connection, &command, HELPER_OUTPUT_BYTES).await?;
    Ok(String::from_utf8_lossy(&output)
        .split_whitespace()
        .next()
        .and_then(|value| value.parse().ok()))
}

async fn sudo_read_chunk(
    connection: &SshConnection,
    path: &str,
    max_bytes: usize,
) -> ApiResult<Vec<u8>> {
    let command = format!("sudo -n head -c {max_bytes} -- {}", shell_quote(path));
    exec_helper(connection, &command, max_bytes).await
}

/// Privileged write by streaming raw bytes into `tee`.
///
/// Raw bytes rather than an encoded payload, so there is no decode step that
/// could alter the content.
async fn sudo_write(
    connection: &SshConnection,
    path: &str,
    data: &[u8],
    append: bool,
) -> ApiResult<()> {
    let flag = if append { "-a " } else { "" };
    let command = format!("sudo -n tee {flag}-- {} > /dev/null", shell_quote(path));
    let mut channel = connection
        .open_exec(&command)
        .await
        .map_err(classify_error)?;
    let transfer = async {
        channel.write_all(data).await.map_err(classify_error)?;
        channel.eof().await.map_err(classify_error)?;
        let (_, stderr, exit, _) = channel
            .collect(HELPER_OUTPUT_BYTES)
            .await
            .map_err(classify_error)?;
        if exit != Some(0) {
            let detail = String::from_utf8_lossy(&stderr).trim().to_owned();
            return Err(ErrorPayload::new(
                "REMOTE_COMMAND_FAILED",
                if detail.is_empty() {
                    format!("privileged write exited with {exit:?}")
                } else {
                    detail
                },
            ));
        }
        Ok(())
    };
    tokio::time::timeout(SUDO_WRITE_TIMEOUT, transfer)
        .await
        .map_err(|_| {
            ErrorPayload::new(
                "REMOTE_TIMEOUT",
                "the privileged write did not finish in time",
            )
        })?
}

async fn sudo_make_dir(connection: &SshConnection, path: &str) -> ApiResult<()> {
    let command = format!("sudo -n mkdir -p -- {}", shell_quote(path));
    exec_helper(connection, &command, HELPER_OUTPUT_BYTES)
        .await
        .map(|_| ())
}

async fn sudo_chmod(connection: &SshConnection, path: &str, mode: u32) -> ApiResult<()> {
    let command = format!("sudo -n chmod {mode:o} -- {}", shell_quote(path));
    exec_helper(connection, &command, HELPER_OUTPUT_BYTES)
        .await
        .map(|_| ())
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

/// Classifies how a channel ended.
///
/// An exit status means the remote shell ran to completion; its absence means
/// the transport went away mid-command, and the remote process state is unknown.
fn channel_end_status(exit: Option<u32>) -> CommandStatus {
    match exit {
        Some(_) => command_status_from_exit(exit),
        None => CommandStatus::Interrupted,
    }
}

pub fn events_response(
    command: Option<CommandInfo>,
    commands: Vec<CommandInfo>,
    events: Vec<TerminalEvent>,
    after: u64,
    more: bool,
    warnings: Vec<String>,
    progress: EventProgress,
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
        progress,
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
                output_bytes: 0,
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

        let page = manager
            .shell_read("historical-session", 0, 64 * 1024, 0)
            .await
            .unwrap();
        assert_eq!(page.commands.len(), 1);
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].payload, b"history");
        assert!(!page.more);
        assert!(!page.progress.timed_out);
        assert_eq!(page.progress.seconds_since_last_output, None);

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
            output_bytes: 0,
        };
        let response = events_response(
            Some(command.clone()),
            vec![command],
            vec![],
            41,
            false,
            vec![],
            EventProgress::default(),
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

    fn test_runtime(session_id: &str, recording_truncated: bool) -> Arc<SessionRuntime> {
        let now = Utc::now();
        Arc::new(SessionRuntime {
            session_id: session_id.into(),
            info: RwLock::new(SessionInfo {
                id: session_id.into(),
                target_id: "target".into(),
                target_name: "Target".into(),
                purpose: "test".into(),
                client_name: "test".into(),
                status: SessionStatus::ExecRunning,
                current_command: None,
                last_exit_code: None,
                recording_truncated,
                host_fingerprint: None,
                created_at: now,
                updated_at: now,
                closed_at: None,
            }),
            connection: Mutex::new(None),
            foreground: Mutex::new(None),
            background: Mutex::new(HashMap::new()),
            sequence: AtomicU64::new(0),
            recording_truncated: AtomicBool::new(recording_truncated),
            live_tail_dropped: AtomicBool::new(false),
            last_activity: StdMutex::new(Instant::now()),
            last_output_at: StdMutex::new(None),
            live_events: StdMutex::new(VecDeque::new()),
            events_tx: watch::channel(0).0,
        })
    }

    /// Builds a manager whose session already has one running command.
    async fn manager_with_running_command(
        recording_limit_mib: u64,
    ) -> (Arc<SessionManager>, Arc<SessionRuntime>, String) {
        let storage =
            Arc::new(Storage::open(std::path::Path::new(":memory:"), recording_limit_mib).unwrap());
        let runtime = test_runtime("live-session", false);
        let manager = SessionManager::new(
            Config::default_config(),
            Paths::under(PathBuf::from("/unused")),
            storage,
        );
        manager
            .sessions
            .write()
            .await
            .insert("live-session".into(), Arc::clone(&runtime));
        let command = CommandInfo {
            id: "live-command".into(),
            session_id: "live-session".into(),
            command: "docker compose build".into(),
            status: CommandStatus::Running,
            exit_code: None,
            started_at: Utc::now(),
            finished_at: None,
            last_sequence: 0,
            recording_truncated: false,
            output_bytes: 0,
        };
        manager
            .storage
            .upsert_session(&runtime.info.read().await.clone())
            .unwrap();
        manager.storage.insert_command(&command).unwrap();
        (manager, runtime, "live-command".into())
    }

    #[test]
    fn treats_a_channel_without_exit_status_as_interrupted() {
        assert_eq!(
            channel_end_status(Some(0)),
            CommandStatus::Completed,
            "a real zero exit is not an interruption"
        );
        assert_eq!(channel_end_status(Some(1)), CommandStatus::Failed);
        assert_eq!(
            channel_end_status(None),
            CommandStatus::Interrupted,
            "no exit status means the transport dropped mid-command"
        );
    }

    #[test]
    fn requires_absolute_local_paths() {
        let error = local_path_for("relative/file").unwrap_err();
        assert_eq!(error.code, "INVALID_ARGUMENT");
        assert_eq!(local_path_for("").unwrap_err().code, "INVALID_ARGUMENT");
        assert_eq!(
            local_path_for("/tmp/out.tgz").unwrap(),
            PathBuf::from("/tmp/out.tgz")
        );
        // HOME is set in any environment that can run these tests.
        if let Some(home) = std::env::var_os("HOME") {
            assert_eq!(
                local_path_for("~/out.tgz").unwrap(),
                PathBuf::from(home).join("out.tgz")
            );
        }
    }

    #[test]
    fn omits_output_age_for_a_finished_command() {
        let command = CommandInfo {
            id: "c".into(),
            session_id: "s".into(),
            command: "true".into(),
            status: CommandStatus::Completed,
            exit_code: Some(0),
            started_at: Utc::now(),
            finished_at: Some(Utc::now()),
            last_sequence: 3,
            recording_truncated: false,
            output_bytes: 7,
        };
        let progress = progress_for(&command, None, false, 4);
        assert_eq!(progress.seconds_since_last_output, None);
        assert_eq!(progress.waited_seconds, 4);
    }

    #[tokio::test]
    async fn a_poll_returns_new_output_immediately() {
        let (manager, runtime, command_id) = manager_with_running_command(1).await;
        manager.record(
            &runtime,
            Some(&command_id),
            StreamKind::Stdout,
            b"layer 1\n".to_vec(),
        );
        let page = manager
            .command_poll(&command_id, 0, 64 * 1024, 30)
            .await
            .unwrap();
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].payload, b"layer 1\n");
        assert!(!page.progress.timed_out);
        assert!(
            page.progress.seconds_since_last_output.is_some(),
            "a live command reports how long it has been quiet"
        );
    }

    #[tokio::test]
    async fn a_poll_blocks_until_output_arrives() {
        let (manager, runtime, command_id) = manager_with_running_command(1).await;
        let recorder = Arc::clone(&manager);
        let target = Arc::clone(&runtime);
        let writer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(120)).await;
            recorder.record(
                &target,
                Some("live-command"),
                StreamKind::Stdout,
                b"late".to_vec(),
            );
        });
        let started = Instant::now();
        let page = manager
            .command_poll(&command_id, 0, 64 * 1024, 30)
            .await
            .unwrap();
        writer.await.unwrap();
        assert_eq!(page.events.len(), 1);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the poll must return as soon as output lands, not after the full budget"
        );
        assert!(!page.progress.timed_out);
    }

    #[tokio::test]
    async fn a_poll_reports_a_timeout_instead_of_blocking_forever() {
        let (manager, _, command_id) = manager_with_running_command(1).await;
        let started = Instant::now();
        let page = manager
            .command_poll(&command_id, 0, 64 * 1024, 1)
            .await
            .unwrap();
        assert!(page.events.is_empty());
        assert!(
            page.progress.timed_out,
            "an expired wait is reported, not silent"
        );
        assert!(page.progress.waited_seconds >= 1);
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "a one second budget must not stretch into the default command timeout"
        );
    }

    #[tokio::test]
    async fn a_zero_wait_never_blocks() {
        let (manager, _, command_id) = manager_with_running_command(1).await;
        let started = Instant::now();
        let page = manager
            .command_poll(&command_id, 0, 64 * 1024, 0)
            .await
            .unwrap();
        assert!(page.events.is_empty());
        assert!(!page.progress.timed_out);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn reports_dropped_live_tail_output() {
        // A zero recording limit rejects every event into the 4 MiB live tail,
        // which then has to discard its oldest entries.
        let (manager, runtime, command_id) = manager_with_running_command(0).await;
        for _ in 0..6 {
            manager.record(
                &runtime,
                Some(&command_id),
                StreamKind::Stdout,
                vec![b'x'; 1024 * 1024],
            );
        }
        assert!(
            runtime.live_tail_dropped.load(Ordering::Relaxed),
            "overflowing the live tail must be recorded rather than silent"
        );
        let page = manager
            .command_poll(&command_id, 0, 48 * 1024, 0)
            .await
            .unwrap();
        assert!(page.progress.live_tail_dropped);
        assert!(
            page.warnings.iter().any(|w| w.contains("overflow buffer")),
            "the caller is told some output is unrecoverable: {:?}",
            page.warnings
        );
    }

    #[tokio::test]
    async fn flags_an_interrupted_command_with_unknown_remote_state() {
        let (manager, runtime, command_id) = manager_with_running_command(1).await;
        manager.record(
            &runtime,
            Some(&command_id),
            StreamKind::Stdout,
            b"half".to_vec(),
        );
        // Simulate the transport dropping: the command is marked interrupted
        // with the output it managed to produce.
        let mut command = manager.storage.command(&command_id).unwrap().unwrap();
        command.status = CommandStatus::Interrupted;
        command.output_bytes = 4;
        manager.storage.update_command(&command).unwrap();

        let page = manager
            .command_poll(&command_id, 0, 64 * 1024, 0)
            .await
            .unwrap();
        assert_eq!(page.command.status, CommandStatus::Interrupted);
        assert!(
            page.warnings
                .iter()
                .any(|warning| warning.contains("remote process started and may still be running")),
            "warnings must distinguish a started process from an unknown one: {:?}",
            page.warnings
        );
    }

    #[tokio::test]
    async fn flags_an_interrupted_command_that_never_produced_output() {
        let (manager, _, command_id) = manager_with_running_command(1).await;
        let mut command = manager.storage.command(&command_id).unwrap().unwrap();
        command.status = CommandStatus::Interrupted;
        manager.storage.update_command(&command).unwrap();
        let page = manager
            .command_poll(&command_id, 0, 64 * 1024, 0)
            .await
            .unwrap();
        assert!(
            page.warnings
                .iter()
                .any(|warning| warning.contains("whether the remote process started is unknown")),
            "{:?}",
            page.warnings
        );
    }

    #[tokio::test]
    async fn lists_commands_across_sessions() {
        let (manager, _, _) = manager_with_running_command(1).await;
        let summaries = manager.commands(None, false, 50).await.unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, "live-command");
        assert_eq!(summaries[0].command_preview, "docker compose build");
        assert!(
            summaries[0].seconds_since_last_output.is_none(),
            "a running command with no output yet has no output age"
        );
    }
}
