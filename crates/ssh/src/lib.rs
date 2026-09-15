use aissh_config::{Auth, Paths, Target};
use anyhow::{Context, Result, anyhow};
use russh::{
    ChannelMsg, Disconnect, client,
    keys::{PrivateKeyWithHashAlg, load_secret_key, ssh_key},
};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

pub mod transfer;

pub use russh_sftp::client::SftpSession;
pub use transfer::{
    RemoteHash, TransferOutcome, download_staged, hash_local_file, hex_digest, make_dir,
    read_capped, sftp_error, stat, try_stat, upload_staged, write_staged,
};

/// How long the SFTP subsystem may take to complete its version handshake.
const SFTP_INIT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone)]
struct ClientHandler {
    fingerprint: Arc<Mutex<Option<String>>>,
}

impl client::Handler for ClientHandler {
    type Error = russh::Error;
    async fn check_server_key(&mut self, key: &ssh_key::PublicKey) -> Result<bool, Self::Error> {
        let value = key.fingerprint(ssh_key::HashAlg::Sha256).to_string();
        if let Ok(mut slot) = self.fingerprint.lock() {
            *slot = Some(value);
        }
        Ok(true)
    }
}

pub struct SshConnection {
    handle: client::Handle<ClientHandler>,
    fingerprint: String,
}
pub enum ChannelEvent {
    Data(Vec<u8>),
    ExtendedData(Vec<u8>),
    ExitStatus(u32),
    Eof,
    Close,
}
pub struct ExecChannel(russh::Channel<client::Msg>);
pub struct PtyReader(russh::ChannelReadHalf);
pub struct PtyWriter(russh::ChannelWriteHalf<client::Msg>);

const UTF8_LOCALE_PRELUDE: &str = "if command -v locale >/dev/null 2>&1; then _aissh_locale=$(locale -a 2>/dev/null | sed -n '/[Uu][Tt][Ff][-_.]*8$/p' | sed -n '1p'); if [ -n \"$_aissh_locale\" ]; then export LANG=\"$_aissh_locale\" LC_ALL=\"$_aissh_locale\"; fi; unset _aissh_locale; fi";
impl SshConnection {
    pub async fn connect(
        target: &Target,
        paths: &Paths,
        timeout: Duration,
        keepalive: Duration,
    ) -> Result<Self> {
        let fingerprint = Arc::new(Mutex::new(None));
        let config = Arc::new(client::Config {
            inactivity_timeout: None,
            keepalive_interval: Some(keepalive),
            keepalive_max: 3,
            ..Default::default()
        });
        let handler = ClientHandler {
            fingerprint: Arc::clone(&fingerprint),
        };
        let mut handle = tokio::time::timeout(
            timeout,
            client::connect(config, (target.host.as_str(), target.port), handler),
        )
        .await
        .context("SSH connection timed out")?
        .context("SSH connection failed")?;
        let auth = match &target.auth {
            Auth::Password { password } => {
                handle
                    .authenticate_password(&target.username, password)
                    .await?
            }
            Auth::PrivateKey { path, passphrase } => {
                let key_path = resolve_key_path(path, paths);
                let key = load_secret_key(&key_path, passphrase.as_deref())
                    .with_context(|| format!("cannot load private key {}", key_path.display()))?;
                let hash = handle.best_supported_rsa_hash().await?.flatten();
                handle
                    .authenticate_publickey(
                        &target.username,
                        PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                    )
                    .await?
            }
        };
        if !auth.success() {
            return Err(anyhow!("AUTH_FAILED: SSH authentication was rejected"));
        }
        let value = fingerprint
            .lock()
            .ok()
            .and_then(|v| v.clone())
            .unwrap_or_else(|| "SHA256:unknown".into());
        Ok(Self {
            handle,
            fingerprint: value,
        })
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
    /// True once the transport is gone. A handle in this state cannot serve any
    /// further channel, so the session layer replaces it before the next call.
    pub fn is_closed(&self) -> bool {
        self.handle.is_closed()
    }
    pub async fn open_exec(&self, command: &str) -> Result<ExecChannel> {
        let channel = self.handle.channel_open_session().await?;
        channel.exec(true, command.as_bytes()).await?;
        Ok(ExecChannel(channel))
    }
    /// Opens a real SFTP subsystem channel.
    ///
    /// A server with the subsystem disabled accepts the channel and then never
    /// completes the protocol handshake, so the failure surfaces here as a
    /// timeout mapped to `SFTP_UNAVAILABLE` rather than as a channel error.
    pub async fn open_sftp(&self) -> Result<SftpSession> {
        let channel = self
            .handle
            .channel_open_session()
            .await
            .map_err(|error| anyhow!("SFTP_UNAVAILABLE: {error}"))?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|error| anyhow!("SFTP_UNAVAILABLE: {error}"))?;
        tokio::time::timeout(
            SFTP_INIT_TIMEOUT,
            SftpSession::new(channel.into_stream()),
        )
        .await
        .map_err(|_| {
            anyhow!("SFTP_UNAVAILABLE: the server did not complete the SFTP handshake")
        })?
        .map_err(|error| {
            anyhow!(
                "SFTP_UNAVAILABLE: {error}; this server may have the sftp subsystem disabled - use ssh_exec_start instead"
            )
        })
    }
    pub async fn open_pty(
        &self,
        cols: u32,
        rows: u32,
        term: &str,
    ) -> Result<(PtyReader, PtyWriter)> {
        let utf8_locale = self.remote_utf8_locale().await;
        let channel = self.handle.channel_open_session().await?;
        if let Some(locale) = utf8_locale {
            // OpenSSH commonly accepts LANG/LC_* through the environment request.
            // Servers that do not accept them simply ignore these best-effort hints.
            channel.set_env(false, "LANG", &locale).await?;
            channel.set_env(false, "LC_ALL", &locale).await?;
        }
        channel
            .request_pty(true, term, cols, rows, 0, 0, &[(russh::Pty::IUTF8, 1)])
            .await?;
        channel.request_shell(true).await?;
        let (reader, writer) = channel.split();
        Ok((PtyReader(reader), PtyWriter(writer)))
    }

    async fn remote_utf8_locale(&self) -> Option<String> {
        let mut channel = self.handle.channel_open_session().await.ok()?;
        channel.exec(true, b"locale -a 2>/dev/null").await.ok()?;
        let mut output = Vec::new();
        while let Some(message) = channel.wait().await {
            match message {
                ChannelMsg::Data { data } => output.extend_from_slice(&data),
                ChannelMsg::Eof | ChannelMsg::Close => break,
                _ => {}
            }
        }
        String::from_utf8(output)
            .ok()?
            .lines()
            .map(str::trim)
            .find(|locale| is_utf8_locale(locale))
            .map(str::to_owned)
    }
    pub async fn disconnect(&self) -> Result<()> {
        self.handle
            .disconnect(Disconnect::ByApplication, "AI SSH session closed", "en")
            .await?;
        Ok(())
    }
}

impl ExecChannel {
    pub async fn next(&mut self) -> Option<ChannelEvent> {
        loop {
            match self.0.wait().await? {
                ChannelMsg::Data { data } => return Some(ChannelEvent::Data(data.to_vec())),
                ChannelMsg::ExtendedData { data, .. } => {
                    return Some(ChannelEvent::ExtendedData(data.to_vec()));
                }
                ChannelMsg::ExitStatus { exit_status } => {
                    return Some(ChannelEvent::ExitStatus(exit_status));
                }
                ChannelMsg::Eof => return Some(ChannelEvent::Eof),
                ChannelMsg::Close => return Some(ChannelEvent::Close),
                _ => {}
            }
        }
    }
    pub async fn cancel(&self) -> Result<()> {
        self.0.signal(russh::Sig::TERM).await?;
        Ok(())
    }
    /// Sends bytes to the remote command's stdin.
    pub async fn write_all(&self, data: &[u8]) -> Result<()> {
        self.0.data_bytes(data.to_vec()).await?;
        Ok(())
    }
    /// Signals end of input so a stdin-consuming remote command can finish.
    pub async fn eof(&self) -> Result<()> {
        self.0.eof().await?;
        Ok(())
    }
    /// Drains stdout, stderr, and the exit status.
    ///
    /// `limit` bounds the bytes retained so a runaway command cannot exhaust
    /// memory; reaching it sets the returned `truncated` flag.
    pub async fn collect(&mut self, limit: usize) -> Result<(Vec<u8>, Vec<u8>, Option<u32>, bool)> {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit = None;
        let mut truncated = false;
        while let Some(event) = self.next().await {
            match event {
                ChannelEvent::Data(data) => {
                    if stdout.len() + data.len() > limit {
                        truncated = true;
                    } else {
                        stdout.extend_from_slice(&data);
                    }
                }
                ChannelEvent::ExtendedData(data) => {
                    if stderr.len() + data.len() > limit {
                        truncated = true;
                    } else {
                        stderr.extend_from_slice(&data);
                    }
                }
                ChannelEvent::ExitStatus(code) => exit = Some(code),
                ChannelEvent::Eof => {}
                ChannelEvent::Close => break,
            }
        }
        Ok((stdout, stderr, exit, truncated))
    }
}

impl PtyReader {
    pub async fn next(&mut self) -> Option<Vec<u8>> {
        loop {
            match self.0.wait().await? {
                ChannelMsg::Data { data } | ChannelMsg::ExtendedData { data, .. } => {
                    return Some(data.to_vec());
                }
                ChannelMsg::Eof | ChannelMsg::Close => return None,
                _ => {}
            }
        }
    }
}

impl PtyWriter {
    pub async fn write(&self, data: &[u8]) -> Result<()> {
        self.0.data_bytes(data.to_vec()).await?;
        Ok(())
    }
    pub async fn resize(&self, cols: u32, rows: u32) -> Result<()> {
        self.0.window_change(cols, rows, 0, 0).await?;
        Ok(())
    }
    pub async fn close(&self) -> Result<()> {
        self.0.eof().await?;
        self.0.close().await?;
        Ok(())
    }
}

pub fn build_command(
    command: &str,
    cwd: Option<&str>,
    env: &BTreeMap<String, String>,
) -> Result<String> {
    for name in env.keys() {
        if name.is_empty()
            || !name
                .bytes()
                .enumerate()
                .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit()))
        {
            return Err(anyhow!(
                "INVALID_ARGUMENT: invalid environment variable name {name:?}"
            ));
        }
    }
    let mut parts = Vec::new();
    if let Some(cwd) = cwd {
        parts.push(format!("cd {}", shell_quote(cwd)));
    }
    if !env.is_empty() {
        parts.push(
            env.iter()
                .map(|(key, value)| format!("export {key}={}", shell_quote(value)))
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    if !env.contains_key("LANG") && !env.contains_key("LC_ALL") {
        parts.push(UTF8_LOCALE_PRELUDE.into());
    }
    // Keep process replacement for cancellation, but let a real shell parse
    // builtins and compound syntax such as `unset`, pipelines, and redirects.
    // A non-login shell also avoids unexpectedly reloading profile proxies.
    parts.push(format!("exec /bin/sh -c {}", shell_quote(command)));
    Ok(parts.join(" && "))
}

/// Quotes a value for a single shell word.
///
/// Public because remote path arguments outside this crate (hashing, `sudo`
/// reads and writes) must reach the shell quoted rather than interpolated.
pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn is_utf8_locale(value: &str) -> bool {
    let value = value.trim().to_ascii_lowercase();
    value.ends_with("utf-8") || value.ends_with("utf8") || value.ends_with("utf_8")
}
fn resolve_key_path(path: &PathBuf, paths: &Paths) -> PathBuf {
    if path.is_absolute() {
        path.clone()
    } else {
        paths.keys.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn builds_scoped_exec_command() {
        let env = BTreeMap::from([("NAME".into(), "a'b".into())]);
        assert_eq!(
            build_command("printf ok", Some("/tmp/a b"), &env).unwrap(),
            "cd '/tmp/a b' && export NAME='a'\\''b' && if command -v locale >/dev/null 2>&1; then _aissh_locale=$(locale -a 2>/dev/null | sed -n '/[Uu][Tt][Ff][-_.]*8$/p' | sed -n '1p'); if [ -n \"$_aissh_locale\" ]; then export LANG=\"$_aissh_locale\" LC_ALL=\"$_aissh_locale\"; fi; unset _aissh_locale; fi && exec /bin/sh -c 'printf ok'"
        );
    }

    #[test]
    fn executes_shell_builtins_and_compound_commands() {
        let env = BTreeMap::from([("LANG".into(), "C".into()), ("NAME".into(), "value".into())]);
        let command = build_command(
            "unset NAME; export RESULT=ok; printf '%s' \"$RESULT:${NAME-unset}\"",
            None,
            &env,
        )
        .unwrap();
        let output = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(command)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"ok:unset");
    }

    #[test]
    fn respects_explicit_locale_environment() {
        let env = BTreeMap::from([("LANG".into(), "zh_CN.GBK".into())]);
        let command = build_command("locale charmap", None, &env).unwrap();
        assert!(!command.contains("_aissh_locale"));
        assert!(command.contains("export LANG='zh_CN.GBK'"));
    }

    #[test]
    fn recognizes_utf8_locale_names() {
        assert!(is_utf8_locale("en_US.UTF-8"));
        assert!(is_utf8_locale("C.utf8"));
        assert!(!is_utf8_locale("zh_CN.GBK"));
    }
    #[test]
    fn rejects_invalid_env_name() {
        assert!(
            build_command("true", None, &BTreeMap::from([("A-B".into(), "x".into())])).is_err()
        );
    }

    #[test]
    fn quotes_paths_for_a_single_shell_word() {
        assert_eq!(shell_quote("/srv/app"), "'/srv/app'");
        assert_eq!(shell_quote("/srv/it's here"), "'/srv/it'\\''s here'");
        assert_eq!(shell_quote("/tmp/a b; rm -rf /"), "'/tmp/a b; rm -rf /'");
    }
}
