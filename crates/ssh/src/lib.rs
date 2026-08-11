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
}
pub struct ExecChannel(russh::Channel<client::Msg>);
pub struct PtyReader(russh::ChannelReadHalf);
pub struct PtyWriter(russh::ChannelWriteHalf<client::Msg>);

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
    pub async fn open_exec(&self, command: &str) -> Result<ExecChannel> {
        let channel = self.handle.channel_open_session().await?;
        channel.exec(true, command.as_bytes()).await?;
        Ok(ExecChannel(channel))
    }
    pub async fn open_pty(
        &self,
        cols: u32,
        rows: u32,
        term: &str,
    ) -> Result<(PtyReader, PtyWriter)> {
        let channel = self.handle.channel_open_session().await?;
        channel
            .request_pty(true, term, cols, rows, 0, 0, &[])
            .await?;
        channel.request_shell(true).await?;
        let (reader, writer) = channel.split();
        Ok((PtyReader(reader), PtyWriter(writer)))
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
                ChannelMsg::Eof | ChannelMsg::Close => return Some(ChannelEvent::Eof),
                _ => {}
            }
        }
    }
    pub async fn cancel(&self) -> Result<()> {
        self.0.signal(russh::Sig::TERM).await?;
        Ok(())
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
    parts.push(format!("exec {command}"));
    Ok(parts.join(" && "))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
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
            "cd '/tmp/a b' && export NAME='a'\\''b' && exec printf ok"
        );
    }
    #[test]
    fn rejects_invalid_env_name() {
        assert!(
            build_command("true", None, &BTreeMap::from([("A-B".into(), "x".into())])).is_err()
        );
    }
}
