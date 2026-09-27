use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{UnixListener, UnixStream};

pub struct Listener {
    listener: UnixListener,
    endpoint: PathBuf,
}

pub struct Stream(UnixStream);

impl Listener {
    pub async fn bind(endpoint: &Path) -> io::Result<Self> {
        if endpoint.exists() {
            match UnixStream::connect(endpoint).await {
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        format!("aisshd is already running at {}", endpoint.display()),
                    ));
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                    ) =>
                {
                    std::fs::remove_file(endpoint).map_err(|error| {
                        io::Error::new(
                            error.kind(),
                            format!("cannot remove stale socket {}: {error}", endpoint.display()),
                        )
                    })?;
                }
                Err(error) => {
                    return Err(io::Error::new(
                        error.kind(),
                        format!(
                            "cannot inspect existing socket {}: {error}",
                            endpoint.display()
                        ),
                    ));
                }
            }
        }
        let listener = UnixListener::bind(endpoint).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("cannot bind daemon socket {}: {error}", endpoint.display()),
            )
        })?;
        Ok(Self {
            listener,
            endpoint: endpoint.to_path_buf(),
        })
    }

    pub async fn accept(&mut self) -> io::Result<Stream> {
        let (stream, _) = self.listener.accept().await?;
        Ok(Stream(stream))
    }

    /// The socket file is created with the process umask, so its mode is set
    /// explicitly once it exists.
    pub fn harden(&self) -> io::Result<()> {
        std::fs::set_permissions(&self.endpoint, std::fs::Permissions::from_mode(0o600))
    }

    pub fn cleanup(&self) {
        let _ = std::fs::remove_file(&self.endpoint);
    }

    pub fn endpoint(&self) -> &Path {
        &self.endpoint
    }
}

impl Stream {
    pub async fn connect(endpoint: &Path) -> io::Result<Self> {
        Ok(Self(UnixStream::connect(endpoint).await?))
    }

    pub fn same_user(&self) -> io::Result<bool> {
        let peer = self.0.peer_cred()?;
        Ok(peer.uid() == unsafe { libc::geteuid() })
    }
}

impl AsyncRead for Stream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        std::pin::Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }
}
