use aissh_protocol::{
    CommandInfo, CommandStatus, SessionInfo, SessionStatus, StreamKind, TerminalEvent,
};
use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use std::{path::Path, sync::Mutex};

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("database lock poisoned")]
    Lock,
}

pub struct Storage {
    connection: Mutex<Connection>,
    recording_limit_bytes: u64,
}

impl Storage {
    pub fn open(path: &Path, recording_limit_mib: u64) -> Result<Self, StorageError> {
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.execute_batch(SCHEMA)?;
        let _ = connection.execute("ALTER TABLE sessions ADD COLUMN last_exit_code INTEGER", []);
        let _ = connection.execute(
            "ALTER TABLE sessions ADD COLUMN recorded_bytes INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = connection.execute(
            "ALTER TABLE sessions ADD COLUMN recording_truncated INTEGER NOT NULL DEFAULT 0",
            [],
        );
        Ok(Self {
            connection: Mutex::new(connection),
            recording_limit_bytes: recording_limit_mib * 1024 * 1024,
        })
    }

    pub fn interrupt_unfinished(&self) -> Result<(), StorageError> {
        let now = Utc::now().to_rfc3339();
        self.conn()?.execute(
            "UPDATE sessions SET status='interrupted',updated_at=?1,closed_at=?1 WHERE status NOT IN ('closed','failed','disconnected','interrupted')",
            [now],
        )?;
        Ok(())
    }

    pub fn upsert_session(&self, session: &SessionInfo) -> Result<(), StorageError> {
        self.conn()?.execute(
            "INSERT INTO sessions (id,target_id,target_name,purpose,client_name,status,current_command,host_fingerprint,created_at,updated_at,closed_at,last_exit_code,recording_truncated)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
             ON CONFLICT(id) DO UPDATE SET status=excluded.status,current_command=excluded.current_command,host_fingerprint=excluded.host_fingerprint,updated_at=excluded.updated_at,closed_at=excluded.closed_at,last_exit_code=excluded.last_exit_code,recording_truncated=(sessions.recording_truncated OR excluded.recording_truncated)",
            params![session.id,session.target_id,session.target_name,session.purpose,session.client_name,enum_text(&session.status),session.current_command,
                session.host_fingerprint,session.created_at.to_rfc3339(),session.updated_at.to_rfc3339(),session.closed_at.map(|v|v.to_rfc3339()),session.last_exit_code,session.recording_truncated],
        )?;
        Ok(())
    }

    pub fn sessions(&self) -> Result<Vec<SessionInfo>, StorageError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare("SELECT id,target_id,target_name,purpose,client_name,status,current_command,host_fingerprint,created_at,updated_at,closed_at,last_exit_code,recording_truncated FROM sessions ORDER BY created_at DESC")?;
        let rows = stmt.query_map([], session_from_row)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn session(&self, id: &str) -> Result<Option<SessionInfo>, StorageError> {
        self.conn()?
            .query_row(
                "SELECT id,target_id,target_name,purpose,client_name,status,current_command,host_fingerprint,created_at,updated_at,closed_at,last_exit_code,recording_truncated FROM sessions WHERE id=?1",
                [id],
                session_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn insert_command(&self, command: &CommandInfo) -> Result<(), StorageError> {
        self.conn()?.execute(
            "INSERT INTO commands (id,session_id,command,status,exit_code,started_at,finished_at,last_sequence,recording_truncated,recorded_bytes) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,0)",
            params![command.id,command.session_id,command.command,enum_text(&command.status),command.exit_code,command.started_at.to_rfc3339(),command.finished_at.map(|v|v.to_rfc3339()),command.last_sequence,command.recording_truncated],
        )?;
        Ok(())
    }

    pub fn update_command(&self, command: &CommandInfo) -> Result<(), StorageError> {
        self.conn()?.execute(
            "UPDATE commands SET status=?2,exit_code=?3,finished_at=?4,last_sequence=?5,recording_truncated=(recording_truncated OR ?6) WHERE id=?1",
            params![command.id,enum_text(&command.status),command.exit_code,command.finished_at.map(|v|v.to_rfc3339()),command.last_sequence,command.recording_truncated],
        )?;
        Ok(())
    }

    pub fn command(&self, id: &str) -> Result<Option<CommandInfo>, StorageError> {
        self.conn()?.query_row(
            "SELECT id,session_id,command,status,exit_code,started_at,finished_at,last_sequence,recording_truncated FROM commands WHERE id=?1",
            [id],
            command_from_row,
        ).optional().map_err(Into::into)
    }

    pub fn latest_command_for_session(
        &self,
        session_id: &str,
    ) -> Result<Option<CommandInfo>, StorageError> {
        self.conn()?.query_row(
            "SELECT id,session_id,command,status,exit_code,started_at,finished_at,last_sequence,recording_truncated FROM commands WHERE session_id=?1 ORDER BY started_at DESC LIMIT 1",
            [session_id],
            command_from_row,
        ).optional().map_err(Into::into)
    }

    pub fn append_event(&self, event: &mut TerminalEvent) -> Result<(), StorageError> {
        let conn = self.conn()?;
        let recorded: u64 = conn.query_row(
            "SELECT recorded_bytes FROM sessions WHERE id=?1",
            [&event.session_id],
            |r| r.get(0),
        )?;
        if recorded.saturating_add(event.payload.len() as u64) > self.recording_limit_bytes {
            event.persisted = false;
            conn.execute(
                "UPDATE sessions SET recording_truncated=1 WHERE id=?1",
                [&event.session_id],
            )?;
            if let Some(id) = &event.command_id {
                conn.execute(
                    "UPDATE commands SET recording_truncated=1 WHERE id=?1",
                    [id],
                )?;
            }
            return Ok(());
        }
        event.persisted = true;
        conn.execute("INSERT INTO events(session_id,command_id,sequence,timestamp,stream,payload) VALUES(?1,?2,?3,?4,?5,?6)",
            params![event.session_id,event.command_id,event.sequence,event.timestamp.to_rfc3339(),enum_text(&event.stream),event.payload])?;
        if let Some(id) = &event.command_id {
            conn.execute(
                "UPDATE commands SET recorded_bytes=recorded_bytes+?2,last_sequence=?3 WHERE id=?1",
                params![id, event.payload.len(), event.sequence],
            )?;
        }
        conn.execute(
            "UPDATE sessions SET recorded_bytes=recorded_bytes+?2 WHERE id=?1",
            params![event.session_id, event.payload.len()],
        )?;
        Ok(())
    }

    pub fn events_for_command(
        &self,
        id: &str,
        after: u64,
        max_bytes: usize,
    ) -> Result<(Vec<TerminalEvent>, bool), StorageError> {
        self.events("command_id", id, after, max_bytes)
    }
    pub fn events_for_session(
        &self,
        id: &str,
        after: u64,
        max_bytes: usize,
    ) -> Result<(Vec<TerminalEvent>, bool), StorageError> {
        self.events("session_id", id, after, max_bytes)
    }

    fn events(
        &self,
        column: &str,
        id: &str,
        after: u64,
        max_bytes: usize,
    ) -> Result<(Vec<TerminalEvent>, bool), StorageError> {
        let conn = self.conn()?;
        // Fetch one row beyond the event-count cap so has_more remains truthful
        // even when many small events fit below max_bytes.
        const PAGE_EVENTS: usize = 2048;
        let sql = format!(
            "SELECT session_id,command_id,sequence,timestamp,stream,payload FROM events WHERE {column}=?1 AND sequence>?2 ORDER BY sequence LIMIT {}",
            PAGE_EVENTS + 1
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![id, after], |row| {
            let stream: String = row.get(4)?;
            Ok(TerminalEvent {
                session_id: row.get(0)?,
                command_id: row.get(1)?,
                sequence: row.get(2)?,
                timestamp: parse_time(row.get(3)?),
                stream: parse_enum(&stream, StreamKind::System),
                payload: row.get(5)?,
                persisted: true,
            })
        })?;
        let (mut result, mut size, mut more) = (Vec::new(), 0, false);
        for row in rows {
            let event = row?;
            if result.len() == PAGE_EVENTS
                || (!result.is_empty() && size + event.payload.len() > max_bytes)
            {
                more = true;
                break;
            }
            size += event.payload.len();
            result.push(event)
        }
        Ok((result, more))
    }

    pub fn cleanup(&self, retention_days: u32) -> Result<usize, StorageError> {
        let before = (Utc::now() - Duration::days(retention_days as i64)).to_rfc3339();
        Ok(self.conn()?.execute("DELETE FROM sessions WHERE created_at<?1 AND status IN ('closed','failed','disconnected','interrupted')",[before])?)
    }

    fn conn(&self) -> Result<std::sync::MutexGuard<'_, Connection>, StorageError> {
        self.connection.lock().map_err(|_| StorageError::Lock)
    }
}

fn enum_text<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value)
        .unwrap_or_default()
        .trim_matches('"')
        .to_owned()
}
fn parse_enum<T: for<'a> serde::Deserialize<'a>>(value: &str, fallback: T) -> T {
    serde_json::from_str(&format!("\"{value}\"")).unwrap_or(fallback)
}
fn parse_time(value: String) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(&value)
        .expect("valid database timestamp")
        .with_timezone(&Utc)
}

fn session_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionInfo> {
    let status: String = row.get(5)?;
    Ok(SessionInfo {
        id: row.get(0)?,
        target_id: row.get(1)?,
        target_name: row.get(2)?,
        purpose: row.get(3)?,
        client_name: row.get(4)?,
        status: parse_enum(&status, SessionStatus::Failed),
        current_command: row.get(6)?,
        last_exit_code: row.get(11)?,
        recording_truncated: row.get(12)?,
        host_fingerprint: row.get(7)?,
        created_at: parse_time(row.get(8)?),
        updated_at: parse_time(row.get(9)?),
        closed_at: row.get::<_, Option<String>>(10)?.map(parse_time),
    })
}

fn command_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CommandInfo> {
    let status: String = row.get(3)?;
    Ok(CommandInfo {
        id: row.get(0)?,
        session_id: row.get(1)?,
        command: row.get(2)?,
        status: parse_enum(&status, CommandStatus::Failed),
        exit_code: row.get(4)?,
        started_at: parse_time(row.get(5)?),
        finished_at: row.get::<_, Option<String>>(6)?.map(parse_time),
        last_sequence: row.get(7)?,
        recording_truncated: row.get(8)?,
    })
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS sessions(id TEXT PRIMARY KEY,target_id TEXT NOT NULL,target_name TEXT NOT NULL,purpose TEXT NOT NULL,client_name TEXT NOT NULL,status TEXT NOT NULL,current_command TEXT,host_fingerprint TEXT,created_at TEXT NOT NULL,updated_at TEXT NOT NULL,closed_at TEXT);
CREATE TABLE IF NOT EXISTS commands(id TEXT PRIMARY KEY,session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,command TEXT NOT NULL,status TEXT NOT NULL,exit_code INTEGER,started_at TEXT NOT NULL,finished_at TEXT,last_sequence INTEGER NOT NULL DEFAULT 0,recording_truncated INTEGER NOT NULL DEFAULT 0,recorded_bytes INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS events(id INTEGER PRIMARY KEY AUTOINCREMENT,session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,command_id TEXT REFERENCES commands(id) ON DELETE CASCADE,sequence INTEGER NOT NULL,timestamp TEXT NOT NULL,stream TEXT NOT NULL,payload BLOB NOT NULL,UNIQUE(session_id,sequence));
CREATE INDEX IF NOT EXISTS events_command_sequence ON events(command_id,sequence);
CREATE INDEX IF NOT EXISTS events_session_sequence ON events(session_id,sequence);
"#;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn truncates_recording_at_limit() {
        let store = Storage::open(Path::new(":memory:"), 0).unwrap();
        let now = Utc::now();
        let session = SessionInfo {
            id: "s".into(),
            target_id: "t".into(),
            target_name: "T".into(),
            purpose: "p".into(),
            client_name: "c".into(),
            status: SessionStatus::Ready,
            current_command: None,
            last_exit_code: None,
            recording_truncated: false,
            host_fingerprint: None,
            created_at: now,
            updated_at: now,
            closed_at: None,
        };
        store.upsert_session(&session).unwrap();
        let stored_session = store.session("s").unwrap().unwrap();
        assert_eq!(stored_session.id, "s");
        assert_eq!(stored_session.status, SessionStatus::Ready);
        let command = CommandInfo {
            id: "c".into(),
            session_id: "s".into(),
            command: "echo".into(),
            status: CommandStatus::Running,
            exit_code: None,
            started_at: now,
            finished_at: None,
            last_sequence: 0,
            recording_truncated: false,
        };
        store.insert_command(&command).unwrap();
        assert_eq!(
            store.latest_command_for_session("s").unwrap().unwrap().id,
            "c"
        );
        let mut event = TerminalEvent {
            session_id: "s".into(),
            command_id: Some("c".into()),
            sequence: 1,
            timestamp: now,
            stream: StreamKind::Stdout,
            payload: vec![1],
            persisted: true,
        };
        store.append_event(&mut event).unwrap();
        assert!(!event.persisted);
        assert!(store.command("c").unwrap().unwrap().recording_truncated)
    }

    #[test]
    fn reports_more_when_event_count_reaches_page_limit() {
        let store = Storage::open(Path::new(":memory:"), 1).unwrap();
        let now = Utc::now();
        let session = SessionInfo {
            id: "s".into(),
            target_id: "t".into(),
            target_name: "T".into(),
            purpose: "p".into(),
            client_name: "c".into(),
            status: SessionStatus::Ready,
            current_command: None,
            last_exit_code: None,
            recording_truncated: false,
            host_fingerprint: None,
            created_at: now,
            updated_at: now,
            closed_at: None,
        };
        store.upsert_session(&session).unwrap();
        let command = CommandInfo {
            id: "c".into(),
            session_id: "s".into(),
            command: "many-events".into(),
            status: CommandStatus::Running,
            exit_code: None,
            started_at: now,
            finished_at: None,
            last_sequence: 0,
            recording_truncated: false,
        };
        store.insert_command(&command).unwrap();
        for sequence in 1..=2050 {
            let mut event = TerminalEvent {
                session_id: "s".into(),
                command_id: Some("c".into()),
                sequence,
                timestamp: now,
                stream: StreamKind::Stdout,
                payload: vec![b'x'],
                persisted: true,
            };
            store.append_event(&mut event).unwrap();
        }

        let (first, has_more) = store.events_for_command("c", 0, 64 * 1024).unwrap();
        assert_eq!(first.len(), 2048);
        assert!(has_more);
        assert_eq!(first.first().unwrap().sequence, 1);
        assert_eq!(first.last().unwrap().sequence, 2048);

        let (second, has_more) = store
            .events_for_command("c", first.last().unwrap().sequence, 64 * 1024)
            .unwrap();
        assert_eq!(second.len(), 2);
        assert!(!has_more);
        assert_eq!(second.last().unwrap().sequence, 2050);
    }
}
